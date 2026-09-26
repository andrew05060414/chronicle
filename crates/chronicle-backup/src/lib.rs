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

pub mod capture;
mod cursor;
mod filehosts;
pub use capture::*;
pub mod components;
pub use components::*;

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
    Pull(PullArgs),
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
    pub dry_run: bool,
    #[arg(long)]
    pub apply_plan: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PullSource {
    #[default]
    Remote,
    Local,
}

impl std::fmt::Display for PullSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Remote => write!(f, "remote"),
            Self::Local => write!(f, "local"),
        }
    }
}

#[derive(Debug, Args, Default)]
pub struct PullArgs {
    pub snapshot_id: Option<String>,
    #[arg(long, value_enum, default_value = "remote")]
    pub from: PullSource,
    /// Restic hostname to pull the latest snapshot from; required when the
    /// repository holds snapshots from more than one machine.
    #[arg(long)]
    pub host: Option<String>,
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
    #[serde(default = "default_component")]
    pub component: ComponentKind,
    pub slot: String,
    pub path: PathBuf,
    #[serde(default)]
    pub host_version: Option<String>,
    #[serde(default)]
    pub host_instance: Option<String>,
    #[serde(default)]
    pub source_kind: SourceKind,
    #[serde(default)]
    pub relative_paths: BTreeMap<String, String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub version_provenance: VersionProvenance,
    #[serde(default)]
    pub capture_status: CaptureStatus,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            app: String::new(),
            component: ComponentKind::Sessions,
            slot: String::new(),
            path: PathBuf::new(),
            host_version: None,
            host_instance: None,
            source_kind: SourceKind::Unknown,
            relative_paths: BTreeMap::new(),
            dependencies: Vec::new(),
            version_provenance: VersionProvenance::Unknown,
            capture_status: CaptureStatus::Unknown,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub relative: String,
    pub source: String,
    pub bytes: u64,
    pub sha256: String,
    pub consistency: String,
    #[serde(default)]
    pub role: FileRole,
    #[serde(default)]
    pub dependencies: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub snapshot_id: String,
    pub device: String,
    pub app_instance: String,
    pub captured_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub sources: Vec<SourceConfig>,
    pub files: Vec<FileEntry>,
    pub exclusions: Vec<String>,
    pub unsupported: Vec<String>,
    pub incremental: bool,
    pub local_restic_snapshot: Option<String>,
    pub remote_restic_snapshot: Option<String>,
    pub remote_confirmed_at: Option<DateTime<Utc>>,
}

impl Manifest {
    pub fn source_for_file(&self, relative_path: &str) -> Option<&SourceConfig> {
        for source in &self.sources {
            let prefix = format!("{}/{}/", source.app, source.slot);
            if relative_path.starts_with(&prefix) {
                return Some(source);
            }
        }
        None
    }

    pub fn files_for_source<'a>(
        &'a self,
        source: &SourceConfig,
    ) -> impl Iterator<Item = &'a FileEntry> {
        let prefix = format!("{}/{}/", source.app, source.slot);
        self.files
            .iter()
            .filter(move |f| f.relative.starts_with(&prefix))
    }

    pub fn source_by_app_and_slot(&self, app: &str, slot: &str) -> Option<&SourceConfig> {
        self.sources.iter().find(|s| s.app == app && s.slot == slot)
    }

    pub fn sources_for_app<'a>(&'a self, app: &'a str) -> impl Iterator<Item = &'a SourceConfig> {
        self.sources.iter().filter(move |s| s.app == app)
    }
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
        NativeCommand::Restore(restore) => restore_snapshot(&root, restore)?,
        NativeCommand::Pull(pull_args) => pull_snapshot(&config, &root, pull_args)?,
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
        ensure_component_supported(&source.app, source.component)?;
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
            ..Default::default()
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
        "antigravity",
        ComponentKind::Sessions,
        "tmp",
        home.join(".gemini/tmp"),
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

fn detect_host_version(
    app: &str,
    source: &Path,
    home: Option<&Path>,
) -> (Option<String>, VersionProvenance) {
    if app == "cursor" {
        let candidates = [
            source
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf),
            home.map(|h| h.join("AppData/Local/Programs/cursor")),
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
                    return (Some(format!("cursor {version}")), VersionProvenance::Probed);
                }
            }
        }
        if home.is_none() {
            if let Some(version) = command_version("cursor") {
                return (Some(format!("cursor {version}")), VersionProvenance::Probed);
            }
            if let Some(h) = dirs::home_dir() {
                let config = h.join("AppData/Local/Programs/cursor");
                for relative in ["resources/app/product.json", "resources/app/package.json"] {
                    if let Ok(value) = fs::read_to_string(config.join(relative))
                        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&value)
                        && let Some(version) =
                            json.get("version").and_then(serde_json::Value::as_str)
                    {
                        return (Some(format!("cursor {version}")), VersionProvenance::Probed);
                    }
                }
            }
        }
        return (None, VersionProvenance::Unknown);
    }
    // If home is explicitly injected (e.g. tests or isolated runs),
    // never execute real host binaries from system PATH!
    if home.is_some() {
        return (None, VersionProvenance::Unknown);
    }
    let command = match app {
        "codex" => "codex",
        "claude-code" => "claude",
        "grok" => "grok",
        "antigravity" => "agy",
        _ => return (None, VersionProvenance::Unknown),
    };
    if let Some(version) = command_version(command) {
        (
            Some(format!("{command} {version}")),
            VersionProvenance::Probed,
        )
    } else {
        (None, VersionProvenance::Unknown)
    }
}

fn command_version(command: &str) -> Option<String> {
    command_version_timeout(command, Duration::from_secs(2))
}

fn capture_sources(
    config: &NativeConfig,
    root: &Path,
    args: &AppArgs,
    home: Option<&Path>,
) -> Result<serde_json::Value> {
    let _lock = exclusive_lock(&root.join("capture.lock"))?;
    let mut sources = selected_sources(config, args, home)?
        .into_iter()
        .map(|mut source| {
            if source.host_version.is_none() {
                let (version, prov) = detect_host_version(&source.app, &source.path, home);
                source.host_version = version;
                source.version_provenance = prov;
            } else if source.version_provenance == VersionProvenance::Unknown {
                source.version_provenance = VersionProvenance::Configured;
            }
            if source.source_kind == SourceKind::Unknown {
                source.source_kind = detect_source_kind(&source.path, &source.app);
            }
            if source.host_instance.is_none() {
                source.host_instance = Some(format!("{}:{}", source.app, source.slot));
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
        for source in &mut sources {
            if !source.path.exists() {
                exclusions.push(format!("{} missing", source.path.display()));
                source.capture_status = CaptureStatus::Failed;
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
            source.capture_status = CaptureStatus::Captured;
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
        let mut watch_queue = load_watch_queue(root).unwrap_or_default();
        // Startup reconciliation happens before waiting for filesystem events.
        match capture_sources(config, root, args, home) {
            Ok(_) => {
                first_event = None;
                watch_queue.pending.clear();
                let _ = save_watch_queue(root, &watch_queue);
                let _ = replication_sender.send(());
            }
            Err(error) => {
                eprintln!("native startup capture pending: {error:#}");
                let now = Utc::now();
                for s in &sources {
                    let key = format!("{}/{}", s.app, s.slot);
                    let entry = watch_queue
                        .pending
                        .entry(key)
                        .or_insert_with(|| WatchQueueEntry {
                            app: s.app.clone(),
                            slot: s.slot.clone(),
                            source_path: s.path.clone(),
                            reasons: vec!["startup reconciliation failed".into()],
                            first_event_at: now,
                            last_event_at: now,
                            attempts: 0,
                            last_error: None,
                        });
                    entry.attempts += 1;
                    entry.last_error = Some(error.to_string());
                    entry.last_event_at = now;
                }
                let _ = save_watch_queue(root, &watch_queue);
            }
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
                    let now_utc = Utc::now();
                    for path in &event.paths {
                        for s in &sources {
                            if path.starts_with(&s.path) {
                                let key = format!("{}/{}", s.app, s.slot);
                                let entry = watch_queue.pending.entry(key).or_insert_with(|| {
                                    WatchQueueEntry {
                                        app: s.app.clone(),
                                        slot: s.slot.clone(),
                                        source_path: s.path.clone(),
                                        reasons: Vec::new(),
                                        first_event_at: now_utc,
                                        last_event_at: now_utc,
                                        attempts: 0,
                                        last_error: None,
                                    }
                                });
                                let reason = format!("{:?}: {}", event.kind, path.display());
                                if !entry.reasons.contains(&reason) {
                                    entry.reasons.push(reason);
                                }
                                entry.last_event_at = now_utc;
                            }
                        }
                    }
                    let _ = save_watch_queue(root, &watch_queue);
                }
                Ok(Err(error)) => {
                    eprintln!("native watch error; reconciliation remains active: {error}");
                    first_event.get_or_insert(Instant::now());
                }
                Err(RecvTimeoutError::Disconnected) => bail!("watch channel disconnected"),
                _ => {}
            }
            if capture_due_at(Instant::now(), last_capture, first_event, last_event) {
                match capture_sources(config, root, args, home) {
                    Ok(_) => {
                        first_event = None;
                        watch_queue.pending.clear();
                        let _ = save_watch_queue(root, &watch_queue);
                        let _ = replication_sender.send(());
                    }
                    Err(error) => {
                        eprintln!("native capture pending: {error:#}");
                        first_event = Some(Instant::now());
                        last_event = Instant::now();
                        let now_utc = Utc::now();
                        for s in &sources {
                            let key = format!("{}/{}", s.app, s.slot);
                            let entry =
                                watch_queue
                                    .pending
                                    .entry(key)
                                    .or_insert_with(|| WatchQueueEntry {
                                        app: s.app.clone(),
                                        slot: s.slot.clone(),
                                        source_path: s.path.clone(),
                                        reasons: vec!["capture failed".into()],
                                        first_event_at: now_utc,
                                        last_event_at: now_utc,
                                        attempts: 0,
                                        last_error: None,
                                    });
                            entry.attempts += 1;
                            entry.last_error = Some(error.to_string());
                            entry.last_event_at = now_utc;
                        }
                        let _ = save_watch_queue(root, &watch_queue);
                    }
                }
                last_capture = Instant::now();
            }
        }
    })
}

pub fn capture_due(
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
    let mut rep_state = load_replication_state(root).unwrap_or_default();
    for manifest in list_manifests(root)? {
        let local_needed = manifest.local_restic_snapshot.is_none();
        let remote_needed = include_remote
            && config.remote_repository.is_some()
            && manifest.remote_restic_snapshot.is_none();
        if local_needed || remote_needed {
            match replicate(&local_config, root, Some(&manifest.snapshot_id)) {
                Ok(_) => {
                    rep_state.failures.remove(&manifest.snapshot_id);
                }
                Err(error) => {
                    let err_str = format!("{:#}", error);
                    let now = Utc::now();
                    let current_manifest =
                        load_manifest(&root.join("snapshots").join(&manifest.snapshot_id))
                            .unwrap_or(manifest);
                    let target = if current_manifest.local_restic_snapshot.is_none() {
                        "local"
                    } else {
                        "remote"
                    };
                    let entry = rep_state
                        .failures
                        .entry(current_manifest.snapshot_id.clone())
                        .or_insert_with(|| ReplicationFailureEntry {
                            target: target.into(),
                            snapshot_id: current_manifest.snapshot_id.clone(),
                            attempts: 0,
                            last_error: err_str.clone(),
                            last_error_at: now,
                            last_success_at: None,
                        });
                    entry.target = target.into();
                    entry.attempts += 1;
                    entry.last_error = err_str.clone();
                    entry.last_error_at = now;
                    failures.push(format!("{}: {err_str}", current_manifest.snapshot_id));
                }
            }
        }
    }
    let _ = save_replication_state(root, &rep_state);
    ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

fn is_shm(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.ends_with("-shm") || s.ends_with(".shm")
}

fn is_wal(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.ends_with("-wal") || s.ends_with(".wal")
}

fn copy_source(
    source: &mut SourceConfig,
    snapshot_root: &Path,
    files: &mut Vec<FileEntry>,
    exclusions: &mut Vec<String>,
) -> Result<()> {
    let root = safe_existing(&source.path)?;
    if is_shm(&root) {
        exclusions.push(format!(
            "{}/{}: shm-not-standalone-artifact",
            source.app, source.slot
        ));
        return Ok(());
    }
    let wal = PathBuf::from(format!("{}-wal", root.display()));
    if wal.is_file() && wal.metadata().is_ok_and(|m| m.len() > 0) {
        let wal_name = wal.file_name().unwrap().to_string_lossy().to_string();
        if !source.dependencies.contains(&wal_name) {
            source.dependencies.push(wal_name);
        }
    }
    if root.is_file() {
        if source.component == ComponentKind::Projects
            && let Err(e) = validate_project_component_file(&root)
        {
            exclusions.push(format!(
                "{}/{}: project-safety-violation: {}",
                source.app, source.slot, e
            ));
            return Ok(());
        }
        if source.component != ComponentKind::Sessions && credential_content(&root) {
            exclusions.push(format!(
                "{}/{}: credential-content",
                source.app, source.slot
            ));
            return Ok(());
        }
        let root_name = root.file_name().unwrap().to_string_lossy();
        if let Some(reason) = filehosts::is_unsupported_path(&source.app, &root_name) {
            exclusions.push(format!("{}/{}: {reason}", source.app, source.slot));
            return Ok(());
        }
        let stored = format!(
            "{}/{}/{}",
            source.app,
            source.slot,
            root.file_name().unwrap().to_string_lossy()
        );
        source.relative_paths.insert(
            stored.clone(),
            root.file_name().unwrap().to_string_lossy().to_string(),
        );
        return copy_one(
            &root,
            &snapshot_root
                .join(&source.app)
                .join(&source.slot)
                .join(root.file_name().unwrap()),
            &stored,
            files,
            exclusions,
            source.dependencies.clone(),
            &source.app,
            &source.slot,
        );
    }
    for entry in WalkDir::new(&root).follow_links(false) {
        let entry = entry?;
        let relative = entry
            .path()
            .strip_prefix(&root)?
            .to_string_lossy()
            .replace('\\', "/");
        if entry.file_type().is_dir() {
            continue;
        }
        if source.component == ComponentKind::Projects
            && let Err(e) = validate_project_component_file(entry.path())
        {
            exclusions.push(format!("{relative}: project-safety-violation: {}", e));
            continue;
        }
        if credential_path(entry.path()) {
            exclusions.push(format!("{relative}: credential"));
            continue;
        }
        if source.component != ComponentKind::Sessions && credential_content(entry.path()) {
            exclusions.push(format!("{relative}: credential-content"));
            continue;
        }
        if is_shm(entry.path()) {
            exclusions.push(format!("{relative}: shm-not-standalone-artifact"));
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
        if let Some(reason) = filehosts::is_unsupported_path(&source.app, &relative) {
            exclusions.push(format!("{relative}: {reason}"));
            continue;
        }
        let stored = format!("{}/{}/{relative}", source.app, source.slot);
        source
            .relative_paths
            .insert(stored.clone(), relative.clone());
        let mut deps = Vec::new();
        let wal = PathBuf::from(format!("{}-wal", entry.path().display()));
        if wal.is_file() && wal.metadata().is_ok_and(|m| m.len() > 0) {
            let wal_name = wal.file_name().unwrap().to_string_lossy().to_string();
            deps.push(wal_name.clone());
            if !source.dependencies.contains(&wal_name) {
                source.dependencies.push(wal_name);
            }
        }
        copy_one(
            entry.path(),
            &snapshot_root
                .join(&source.app)
                .join(&source.slot)
                .join(&relative),
            &stored,
            files,
            exclusions,
            deps,
            &source.app,
            &source.slot,
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
    dependencies: Vec<String>,
    app: &str,
    slot: &str,
) -> Result<()> {
    if credential_path(source) {
        exclusions.push(format!("{stored}: credential"));
        return Ok(());
    }
    if is_shm(source) {
        exclusions.push(format!("{stored}: shm-not-standalone-artifact"));
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

    // Mid-copy truncate/replace detection: take stamp before copying
    let stamp_before = SourceFileStamp::of(source)?;

    let consistency = if stored.starts_with("cursor/") && sqlite_path(source) {
        let omitted = cursor::backup_sessions(source, destination)?;
        exclusions.push(format!("{stored}: {omitted} non-session rows excluded"));
        if let Err(err) = cursor::audit_projection(destination) {
            exclusions.push(format!("{stored}: integrity: {err:#}"));
        }
        "sqlite-session-projection"
    } else if sqlite_path(source) {
        backup_sqlite(source, destination)?;
        "sqlite-consistent"
    } else {
        fs::copy(source, destination)?;
        "file-copy"
    };

    // Assert file was not modified or replaced during copy
    stamp_before.assert_unmodified(source)?;

    let (bytes, sha) = hash_file(destination)?;
    let role = detect_file_role(stored, app, slot);
    files.push(FileEntry {
        relative: stored.replace('\\', "/"),
        source: source.display().to_string(),
        bytes,
        sha256: sha,
        consistency: consistency.into(),
        role,
        dependencies,
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

fn pull_snapshot(config: &NativeConfig, root: &Path, args: PullArgs) -> Result<serde_json::Value> {
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
    let repo = match args.from {
        PullSource::Remote => config
            .remote_repository
            .as_ref()
            .context("remote repository is not configured")?,
        PullSource::Local => config
            .local_repository
            .as_ref()
            .context("local repository is not configured")?,
    };

    let _lock = exclusive_lock(&root.join("pull.lock"))?;

    let snapshots_val = restic_json(
        &restic,
        &password,
        root,
        &["-r", repo, "snapshots", "--json"],
    )?;
    let items = snapshots_val
        .as_array()
        .context("restic snapshots returned non-array JSON")?;

    let (snapshot_id, restic_id) = choose_pull_snapshot(
        items,
        args.snapshot_id.as_deref(),
        args.host.as_deref(),
        repo,
    )?;

    let snapshots_root = root.join("snapshots");
    fs::create_dir_all(&snapshots_root)?;
    let dest_dir = snapshots_root.join(&snapshot_id);

    if dest_dir.exists() {
        let manifest = verify_local(root, &snapshot_id).map_err(|err| {
            anyhow::anyhow!(
                "local snapshot '{snapshot_id}' already exists but failed verification: {err}; manual resolution required"
            )
        })?;
        return Ok(serde_json::json!({
            "mode": "already-present",
            "snapshot_id": snapshot_id,
            "from": args.from.to_string(),
            "restic_snapshot": restic_id,
            "files": manifest.files.len(),
            "next": "restore <id> --target <dir>",
        }));
    }

    let timestamp = Utc::now().format("%Y%m%d%H%M%S%3f").to_string();
    let staging_root = root.join("staging");
    fs::create_dir_all(&staging_root)?;
    let staging_dir = staging_root.join(format!("pull-{snapshot_id}-{timestamp}"));

    let restore_res = restic_command(
        &restic,
        &password,
        root,
        &[
            "-r",
            repo,
            "restore",
            &restic_id,
            "--target",
            &staging_dir.display().to_string(),
        ],
    );
    if let Err(err) = restore_res {
        let _ = fs::remove_dir_all(&staging_dir);
        return Err(err);
    }

    let mut found_dir: Option<PathBuf> = None;
    for entry in WalkDir::new(&staging_dir)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_type().is_dir()
            && entry.file_name() == std::ffi::OsStr::new(&snapshot_id)
            && entry.path().join("manifest.json").is_file()
        {
            found_dir = Some(entry.path().to_path_buf());
            break;
        }
    }

    let found_dir = match found_dir {
        Some(dir) => dir,
        None => {
            let _ = fs::remove_dir_all(&staging_dir);
            bail!(
                "restored snapshot '{snapshot_id}' directory with manifest.json not found in staging tree at {}",
                staging_dir.display()
            );
        }
    };

    if let Err(err) = fs::rename(&found_dir, &dest_dir) {
        let _ = fs::remove_dir_all(&staging_dir);
        return Err(err.into());
    }

    match verify_local(root, &snapshot_id) {
        Ok(manifest) => {
            let _ = fs::remove_dir_all(&staging_dir);
            Ok(serde_json::json!({
                "mode": "pulled",
                "snapshot_id": snapshot_id,
                "from": args.from.to_string(),
                "restic_snapshot": restic_id,
                "files": manifest.files.len(),
                "next": "restore <id> --target <dir>",
            }))
        }
        Err(err) => {
            let _ = fs::remove_dir_all(&staging_dir);
            let failed_dir = staging_root.join(format!("failed-pull-{snapshot_id}-{timestamp}"));
            let _ = fs::rename(&dest_dir, &failed_dir);
            bail!(
                "pulled snapshot '{snapshot_id}' failed local verification: {err}; isolated to {}",
                failed_dir.display()
            );
        }
    }
}

/// Pick `(chronicle_id, restic_id)` from `restic snapshots --json` output.
fn choose_pull_snapshot(
    items: &[serde_json::Value],
    snapshot_id: Option<&str>,
    host: Option<&str>,
    repo: &str,
) -> Result<(String, String)> {
    struct Candidate {
        restic_id: String,
        chronicle_id: String,
        hostname: String,
        time: DateTime<Utc>,
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut available_ids: BTreeSet<String> = BTreeSet::new();

    for item in items {
        let restic_id = match item.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };
        let hostname = item
            .get("hostname")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let time = item.get("time").and_then(|v| v.as_str()).and_then(|s| {
            DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        });

        if let Some(tags) = item.get("tags").and_then(|v| v.as_array()) {
            for tag in tags {
                if let Some(tag_str) = tag.as_str()
                    && let Some(chronicle_id) = tag_str.strip_prefix("chronicle-native:")
                    && uuid::Uuid::parse_str(chronicle_id).is_ok()
                {
                    available_ids.insert(chronicle_id.to_string());
                    if let Some(time) = time {
                        candidates.push(Candidate {
                            restic_id: restic_id.clone(),
                            chronicle_id: chronicle_id.to_string(),
                            hostname: hostname.clone(),
                            time,
                        });
                    }
                }
            }
        }
    }

    let chosen = match snapshot_id {
        Some(target_id) => {
            let mut matching: Vec<Candidate> = candidates
                .into_iter()
                .filter(|c| c.chronicle_id == target_id)
                .collect();
            if matching.is_empty() {
                let available_list: Vec<String> = available_ids.into_iter().take(20).collect();
                if available_list.is_empty() {
                    bail!(
                        "snapshot '{target_id}' not found in repository '{repo}': no chronicle-native snapshots available"
                    );
                } else {
                    bail!(
                        "snapshot '{target_id}' not found in repository '{repo}'; available chronicle-native snapshots (up to 20): {}",
                        available_list.join(", ")
                    );
                }
            }
            matching.sort_by_key(|b| std::cmp::Reverse(b.time));
            matching.swap_remove(0)
        }
        None => {
            if let Some(host) = host {
                candidates.retain(|c| c.hostname == host);
            }
            if candidates.is_empty() {
                bail!("no chronicle-native snapshots found in repository '{repo}'");
            }
            // Never guess across machines: the newest snapshot may belong to another device.
            let hosts: BTreeSet<&str> = candidates.iter().map(|c| c.hostname.as_str()).collect();
            if hosts.len() > 1 {
                bail!(
                    "repository '{repo}' holds snapshots from several machines ({}); pass --host or a snapshot id",
                    hosts.into_iter().collect::<Vec<_>>().join(", ")
                );
            }
            candidates.sort_by_key(|b| std::cmp::Reverse(b.time));
            candidates.swap_remove(0)
        }
    };

    Ok((chosen.chronicle_id, chosen.restic_id))
}

fn restore_snapshot(root: &Path, args: RestoreArgs) -> Result<serde_json::Value> {
    if let Some(plan_path) = args.apply_plan {
        let plan = load_extract_plan(root, &plan_path)?;
        if args.dry_run {
            return Ok(serde_json::json!({"mode": "apply-dry-run", "plan": plan}));
        }
        return apply_extract_plan(root, plan);
    }
    let id = args.snapshot_id.context("snapshot id required")?;
    let manifest = verify_local(root, &id)?;
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

#[cfg(test)]
pub(crate) fn apply_saved_plan(root: &Path, path: &Path) -> Result<serde_json::Value> {
    apply_extract_plan(root, load_extract_plan(root, path)?)
}

/// Load a saved extraction plan and check it still matches its snapshot, so a
/// dry run reports the same refusal an apply would.
fn load_extract_plan(root: &Path, path: &Path) -> Result<ExtractPlan> {
    let plan: ExtractPlan = serde_json::from_str(&fs::read_to_string(path)?)?;
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
    ensure!(
        plan.target_fingerprint == fingerprint_path(&plan.target)?,
        "restore target changed; generate a new plan"
    );
    Ok(plan)
}

fn apply_extract_plan(root: &Path, plan: ExtractPlan) -> Result<serde_json::Value> {
    extract_snapshot(root, &plan.snapshot_id, &plan.target)?;
    Ok(
        serde_json::json!({"mode":"applied-files","snapshot_id":plan.snapshot_id,"target":plan.target,"client_index":"not-updated; native client installation is not part of this release"}),
    )
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
    let watch_queue = load_watch_queue(root).unwrap_or_default();
    let rep_state = load_replication_state(root).unwrap_or_default();
    Ok(serde_json::json!({
        "local_snapshot": latest.map(|m| &m.snapshot_id),
        "local_restic_snapshot": latest.and_then(|m| m.local_restic_snapshot.clone()),
        "remote_confirmed_at": latest.and_then(|m| m.remote_confirmed_at),
        "remote_unknown_when_absent": latest.is_some_and(|m| m.remote_confirmed_at.is_none()),
        "watch_queue": {
            "pending_count": watch_queue.pending.len(),
            "pending": watch_queue.pending,
        },
        "replication_failures": {
            "failure_count": rep_state.failures.len(),
            "failures": rep_state.failures,
        },
    }))
}

fn list_manifests(root: &Path) -> Result<Vec<Manifest>> {
    let mut values = Vec::new();
    let snapshots = root.join("snapshots");
    if !snapshots.is_dir() {
        return Ok(values);
    }
    for entry in fs::read_dir(snapshots)? {
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
                        if is_wal(entry.path()) && entry.metadata().is_ok_and(|m| m.len() == 0) {
                            continue;
                        }
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
                    if wal.is_file() && wal.metadata().is_ok_and(|m| m.len() > 0) {
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
            if source.path.is_file() {
                let wal = PathBuf::from(format!("{}-wal", source.path.display()));
                if wal.is_file() && wal.metadata().is_ok_and(|m| m.len() > 0) {
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
        if is_wal(path) && path.metadata().is_ok_and(|m| m.len() == 0) {
            return Ok(());
        }
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
            if is_wal(entry.path()) && entry.metadata().is_ok_and(|m| m.len() == 0) {
                continue;
            }
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
        return true;
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
                    !meta.is_symlink() || is_macos_system_store_alias(ancestor),
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
#[cfg(target_os = "macos")]
fn is_macos_system_store_alias(path: &Path) -> bool {
    let expected = match path {
        p if p == Path::new("/var") => Some(Path::new("/private/var")),
        p if p == Path::new("/tmp") => Some(Path::new("/private/tmp")),
        _ => None,
    };
    expected
        .is_some_and(|expected| fs::canonicalize(path).is_ok_and(|resolved| resolved == expected))
}
#[cfg(not(target_os = "macos"))]
fn is_macos_system_store_alias(_path: &Path) -> bool {
    false
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
/// `CHRONICLE_NATIVE_ROOT`, else the platform data directory. Set `data_root`
/// in the native config to keep backups on a different disk.
fn default_root() -> PathBuf {
    if let Some(root) = std::env::var_os("CHRONICLE_NATIVE_ROOT").filter(|v| !v.is_empty()) {
        return PathBuf::from(root);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("chronicle-native")
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
    let mut file = File::create(&temp)?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    drop(file);
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

    /// Restic for round-trip tests: `CHRONICLE_TEST_RESTIC`, else `restic` on PATH.
    /// Skips when absent unless `CHRONICLE_REQUIRE_RESTIC=1` (set in CI).
    fn test_restic() -> Option<PathBuf> {
        let candidate = std::env::var_os("CHRONICLE_TEST_RESTIC")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("restic"));
        let works = Command::new(&candidate)
            .arg("version")
            .output()
            .is_ok_and(|output| output.status.success());
        if works {
            return Some(candidate);
        }
        assert!(
            std::env::var_os("CHRONICLE_REQUIRE_RESTIC").is_none(),
            "restic is required for this test run but was not found"
        );
        eprintln!("skipping restic round trip: restic not found");
        None
    }

    fn restic_item(id: &str, host: &str, time: &str, tag: &str) -> serde_json::Value {
        serde_json::json!({"id": id, "hostname": host, "time": time, "tags": [tag]})
    }

    #[test]
    fn pull_selection_never_guesses_across_machines() {
        let a = "0afca009-7911-4ed8-8121-4601a64aa5bd";
        let b = "1b2c3d4e-7911-4ed8-8121-4601a64aa5bd";
        let items = vec![
            restic_item(
                "r1",
                "laptop",
                "2026-09-20T10:00:00Z",
                &format!("chronicle-native:{a}"),
            ),
            restic_item(
                "r2",
                "desktop",
                "2026-09-21T10:00:00Z",
                &format!("chronicle-native:{b}"),
            ),
            restic_item(
                "r3",
                "desktop",
                "2026-09-22T10:00:00Z",
                "chronicle-native:../../escape",
            ),
        ];

        let err = choose_pull_snapshot(&items, None, None, "repo").unwrap_err();
        assert!(err.to_string().contains("several machines"), "{err}");

        assert_eq!(
            choose_pull_snapshot(&items, None, Some("laptop"), "repo").unwrap(),
            (a.to_string(), "r1".to_string())
        );
        assert_eq!(
            choose_pull_snapshot(&items, Some(b), None, "repo").unwrap(),
            (b.to_string(), "r2".to_string())
        );
        // Tags that are not snapshot UUIDs are never selectable.
        assert!(choose_pull_snapshot(&items, Some("../../escape"), None, "repo").is_err());
        assert!(choose_pull_snapshot(&items, None, Some("nowhere"), "repo").is_err());
    }
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
                ..Default::default()
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
        assert!(apply_saved_plan(&root, &plan_path).is_err());
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

    #[cfg(target_os = "macos")]
    #[test]
    fn validate_store_allows_macos_temp_root_but_rejects_other_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        validate_store(temp.path()).unwrap();

        let link = temp.path().join("store-link");
        std::os::unix::fs::symlink(temp.path(), &link).unwrap();
        assert!(validate_store(&link).is_err());
    }

    #[test]
    fn default_config_selects_only_sessions() {
        let home = tempfile::tempdir().unwrap();
        let codex_sessions = home.path().join(".codex/sessions");
        fs::create_dir_all(&codex_sessions).unwrap();
        let sources = selected_sources(
            &NativeConfig::default(),
            &AppArgs::default(),
            Some(home.path()),
        )
        .unwrap();
        assert!(!sources.is_empty());
        for source in sources {
            assert_eq!(source.component, ComponentKind::Sessions);
        }
    }

    #[test]
    fn unsupported_host_component_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("backup");
        let source_path = temp.path().join("plugins");
        fs::create_dir_all(&source_path).unwrap();
        fs::write(source_path.join("config.json"), b"{}").unwrap();
        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "codex".into(),
                component: ComponentKind::Plugins,
                slot: "plugins".into(),
                path: source_path,
                host_version: Some("codex 1.0".into()),
                ..Default::default()
            }],
            components: vec![ComponentKind::Plugins],
            ..Default::default()
        };
        let err = capture_sources(&config, &root, &AppArgs::default(), None).unwrap_err();
        assert!(err.to_string().contains("codex"));
        assert!(err.to_string().contains("Plugins") || err.to_string().contains("plugins"));
        assert!(!root.join("snapshots").exists());
    }

    #[test]
    fn projects_component_captures_only_metadata_never_source_or_git() {
        let temp = tempfile::tempdir().unwrap();
        let proj_dir = temp.path().join("my-project");
        fs::create_dir_all(proj_dir.join("src")).unwrap();
        fs::create_dir_all(proj_dir.join(".git")).unwrap();
        fs::write(
            proj_dir.join("project.json"),
            b"{\"id\":\"p1\",\"workspace_path\":\"/my/work\"}",
        )
        .unwrap();
        fs::write(proj_dir.join("src/main.rs"), b"fn main() {}").unwrap();
        fs::write(proj_dir.join(".git/config"), b"[core]").unwrap();

        let root = temp.path().join("backup");
        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "codex".into(),
                component: ComponentKind::Projects,
                slot: "projects".into(),
                path: proj_dir,
                host_version: Some("fixture-v1".into()),
                ..Default::default()
            }],
            components: vec![ComponentKind::Projects],
            ..Default::default()
        };

        let captured = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        assert_eq!(captured["files"], 1);
        let snapshot = root
            .join("snapshots")
            .join(captured["snapshot_id"].as_str().unwrap());
        assert!(snapshot.join("codex/projects/project.json").is_file());
        assert!(!snapshot.join("codex/projects/src/main.rs").exists());
        assert!(!snapshot.join("codex/projects/.git/config").exists());
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
        fs::write(
            source.join("auth-header.json"),
            b"{\"theme\":\"dark\",\"headers\":{\"Authorization\":\"Bearer secret-token\"}}",
        )
        .unwrap();
        fs::write(
            source.join("cookie.json"),
            b"{\"theme\":\"dark\",\"cookie\":\"session=xyz123\"}",
        )
        .unwrap();
        fs::write(source.join("plugin.js"), b"const password = 'FAKE_SECRET';").unwrap();

        // Small SQLite db containing a token row
        let mixed_db = source.join("mixed.sqlite");
        let conn = Connection::open(&mixed_db).unwrap();
        conn.execute_batch("CREATE TABLE credentials(service TEXT, token TEXT); INSERT INTO credentials VALUES ('auth', 'secret-val');").unwrap();
        drop(conn);

        let root = temp.path().join("backup");
        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "claude-code".into(),
                component: ComponentKind::Settings,
                slot: "settings".into(),
                path: source,
                host_version: Some("fixture-v1".into()),
                ..Default::default()
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
        assert!(snapshot.join("claude-code/settings/safe.json").is_file());
        assert!(
            !snapshot
                .join("claude-code/settings/looks-safe.json")
                .exists()
        );
        assert!(
            !snapshot
                .join("claude-code/settings/auth-header.json")
                .exists()
        );
        assert!(!snapshot.join("claude-code/settings/cookie.json").exists());
        assert!(!snapshot.join("claude-code/settings/plugin.js").exists());
        assert!(!snapshot.join("claude-code/settings/mixed.sqlite").exists());
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
                ..Default::default()
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
                ..Default::default()
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
            ..Default::default()
        };
        let password = temp.path().join("password");
        fs::write(&password, "synthetic-test-password").unwrap();
        let local_repo = temp.path().join("local-repo");
        let remote_repo = temp.path().join("remote-repo");
        let Some(restic) = test_restic() else {
            return;
        };
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

    #[test]
    fn dry_run_makes_zero_target_writes() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let source_file = home.join("source/session.jsonl");
        fs::create_dir_all(source_file.parent().unwrap()).unwrap();
        fs::write(&source_file, b"content").unwrap();
        let root = temp.path().join("backup");
        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "grok".into(),
                component: ComponentKind::Sessions,
                slot: "sessions".into(),
                path: source_file,
                host_version: Some("fixture-v1".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let captured = capture_sources(&config, &root, &AppArgs::default(), Some(&home)).unwrap();
        let id = captured["snapshot_id"].as_str().unwrap();

        let target_dir = home.join(".grok/sessions");
        assert!(!target_dir.exists());

        let extract_args = RestoreArgs {
            snapshot_id: Some(id.to_string()),
            target: Some(target_dir.clone()),
            dry_run: true,
            apply_plan: None,
        };
        let res = restore_snapshot(&root, extract_args).unwrap();
        assert_eq!(res["mode"], "extract-dry-run");
        assert!(
            !target_dir.exists(),
            "extract dry run must make ZERO writes"
        );
        assert!(
            !root.join("plans").exists(),
            "dry run must not write plan files"
        );
    }

    #[test]
    fn explain_snapshot_after_deleting_synthetic_sources_from_snapshot_only() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let codex_dir = home.join("codex_sessions");
        let cursor_dir = home.join("cursor_data");
        let claude_dir = home.join("claude_projects");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::create_dir_all(&cursor_dir).unwrap();
        fs::create_dir_all(&claude_dir).unwrap();

        // 1. Codex session rollout
        fs::write(
            codex_dir.join("session-1.jsonl"),
            b"{\"type\":\"session_meta\"}\n",
        )
        .unwrap();
        // 2. Cursor SQLite session database
        let cursor_db = cursor_dir.join("state.vscdb");
        let conn = Connection::open(&cursor_db).unwrap();
        conn.execute_batch("CREATE TABLE ItemTable(key TEXT PRIMARY KEY, value TEXT); INSERT INTO ItemTable VALUES('composerData:c1', 'hello');").unwrap();
        drop(conn);
        // 3. Claude-code project session
        fs::write(claude_dir.join("proj.jsonl"), b"{\"project\":\"test\"}\n").unwrap();

        let root = temp.path().join("backup");
        let config = NativeConfig {
            data_root: Some(root.clone()),
            sources: vec![
                SourceConfig {
                    app: "codex".into(),
                    component: ComponentKind::Sessions,
                    slot: "sessions".into(),
                    path: codex_dir.clone(),
                    host_version: Some("fixture-v1".into()),
                    ..Default::default()
                },
                SourceConfig {
                    app: "cursor".into(),
                    component: ComponentKind::Sessions,
                    slot: "global-storage".into(),
                    path: cursor_db.clone(),
                    host_version: Some("fixture-v1".into()),
                    ..Default::default()
                },
                SourceConfig {
                    app: "claude-code".into(),
                    component: ComponentKind::Sessions,
                    slot: "projects".into(),
                    path: claude_dir.clone(),
                    host_version: Some("fixture-v1".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let captured = capture_sources(&config, &root, &AppArgs::default(), Some(&home)).unwrap();
        let snapshot_id = captured["snapshot_id"].as_str().unwrap();
        let snapshot_dir = root.join("snapshots").join(snapshot_id);

        // HARD REQUIREMENT: DELETE the synthetic sources!
        fs::remove_dir_all(&home).unwrap();
        assert!(!home.exists());
        assert!(!codex_dir.exists());
        assert!(!cursor_db.exists());
        assert!(!claude_dir.exists());

        // Explain snapshot entirely from snapshot_dir alone
        let explanation = explain_snapshot(&snapshot_dir).unwrap();
        assert_eq!(explanation.snapshot_id, snapshot_id);
        assert_eq!(explanation.sources.len(), 3);

        // Check codex source & file
        let codex_src = explanation
            .sources
            .iter()
            .find(|s| s.app == "codex")
            .unwrap();
        assert_eq!(codex_src.source_kind, SourceKind::FileTree);
        assert_eq!(codex_src.component, ComponentKind::Sessions);
        assert_eq!(codex_src.host_version.as_deref(), Some("fixture-v1"));
        assert_eq!(codex_src.version_provenance, VersionProvenance::Configured);
        assert_eq!(codex_src.capture_status, CaptureStatus::Captured);
        let codex_file = explanation.files.iter().find(|f| f.app == "codex").unwrap();
        assert_eq!(codex_file.role, FileRole::SessionRollout);
        assert!(codex_file.relative_path.contains("session-1.jsonl"));

        // Check cursor source & file
        let cursor_src = explanation
            .sources
            .iter()
            .find(|s| s.app == "cursor")
            .unwrap();
        assert_eq!(cursor_src.source_kind, SourceKind::SqliteSessionProjection);
        assert_eq!(cursor_src.component, ComponentKind::Sessions);
        let cursor_file = explanation
            .files
            .iter()
            .find(|f| f.app == "cursor")
            .unwrap();
        assert_eq!(cursor_file.role, FileRole::SessionProjection);
        assert!(cursor_file.relative_path.contains("state.vscdb"));

        // Check claude-code source & file
        let claude_src = explanation
            .sources
            .iter()
            .find(|s| s.app == "claude-code")
            .unwrap();
        assert_eq!(claude_src.source_kind, SourceKind::FileTree);
        let claude_file = explanation
            .files
            .iter()
            .find(|f| f.app == "claude-code")
            .unwrap();
        assert_eq!(claude_file.role, FileRole::SessionRollout);

        // Old manifests without new fields must still deserialize and report unknown
        let old_manifest_json = serde_json::json!({
            "version": 1,
            "snapshot_id": "00000000-0000-0000-0000-000000000001",
            "device": "old-device",
            "app_instance": "chronicle-native",
            "captured_at": "2026-09-22T00:00:00Z",
            "finished_at": "2026-09-22T00:01:00Z",
            "sources": [
                {
                    "app": "codex",
                    "component": "sessions",
                    "slot": "sessions",
                    "path": "C:/lost/path",
                    "host_version": "1.0.0"
                }
            ],
            "files": [
                {
                    "relative": "codex/sessions/rollout.jsonl",
                    "source": "C:/lost/path/rollout.jsonl",
                    "bytes": codex_file.bytes,
                    "sha256": codex_file.sha256,
                    "consistency": "file-copy"
                }
            ],
            "exclusions": [],
            "unsupported": [],
            "incremental": false
        });
        let old_manifest: Manifest = serde_json::from_value(old_manifest_json).unwrap();
        assert_eq!(old_manifest.sources[0].source_kind, SourceKind::Unknown);
        assert_eq!(
            old_manifest.sources[0].version_provenance,
            VersionProvenance::Unknown
        );
        assert_eq!(
            old_manifest.sources[0].capture_status,
            CaptureStatus::Unknown
        );
        assert_eq!(old_manifest.files[0].role, FileRole::Unknown);
    }

    #[test]
    fn truncate_or_replace_during_capture_detects_change_and_cleans_up_staging() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("active_session.jsonl");
        fs::write(&file_path, b"initial content").unwrap();

        // SourceFileStamp detects truncation / size change
        let stamp = SourceFileStamp::of(&file_path).unwrap();
        assert!(stamp.assert_unmodified(&file_path).is_ok());

        // Modify file size
        fs::write(&file_path, b"new longer content here").unwrap();
        assert!(stamp.assert_unmodified(&file_path).is_err());

        // Verify staging is deleted on error and no incomplete snapshot is published
        let root = temp.path().join("backup");
        let config = NativeConfig {
            data_root: Some(root.clone()),
            sources: vec![SourceConfig {
                app: "fixture".into(),
                component: ComponentKind::Sessions,
                slot: "sessions".into(),
                path: file_path.clone(),
                host_version: Some("fixture-v1".into()),
                ..Default::default()
            }],
            ..Default::default()
        };

        // Missing file during capture: tracked in exclusions, doesn't panic
        fs::remove_file(&file_path).unwrap();
        let res = capture_sources(&config, &root, &AppArgs::default(), None);
        assert!(res.is_ok());
        assert_eq!(res.unwrap()["files"], 0);

        // Staging directory must be empty
        if root.join("staging").exists() {
            assert_eq!(fs::read_dir(root.join("staging")).unwrap().count(), 0);
        }
    }

    #[test]
    fn wal_only_change_triggers_new_capture_and_shm_never_captured() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir_all(&source_root).unwrap();

        let db_path = source_root.join("test.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE t(v TEXT); INSERT INTO t VALUES('v1');").unwrap();
        drop(conn);

        // Put an -shm file in source
        let shm_path = source_root.join("test.db-shm");
        fs::write(&shm_path, b"dummy shm").unwrap();

        let conn = Connection::open(&db_path).unwrap();

        let root = temp.path().join("backup");
        let config = NativeConfig {
            data_root: Some(root.clone()),
            sources: vec![SourceConfig {
                app: "sqlite_app".into(),
                component: ComponentKind::Sessions,
                slot: "db".into(),
                path: source_root.clone(),
                host_version: Some("fixture-v1".into()),
                ..Default::default()
            }],
            ..Default::default()
        };

        // First capture
        let first = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        assert_eq!(first["status"], "captured");
        let id1 = first["snapshot_id"].as_str().unwrap();
        let snap1 = root.join("snapshots").join(id1);

        // SHM MUST NOT be captured as a recovery artifact!
        assert!(!snap1.join("sqlite_app/db/test.db-shm").exists());
        assert!(snap1.join("sqlite_app/db/test.db").exists());

        // Unchanged capture
        let second = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        assert_eq!(second["status"], "unchanged");

        // Now modify ONLY the WAL (via SQL insert without checkpoint)
        let before_db_hash = hash_file(&db_path).unwrap();
        conn.execute("INSERT INTO t VALUES('v2')", []).unwrap();
        conn.cache_flush().unwrap();
        // The main DB file is untouched because WAL has not checkpointed
        assert_eq!(hash_file(&db_path).unwrap(), before_db_hash);
        assert!(source_root.join("test.db-wal").exists());

        // WAL-only change MUST trigger a new capture!
        let third = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        assert_eq!(third["status"], "captured");
        let id3 = third["snapshot_id"].as_str().unwrap();
        assert_ne!(id1, id3);

        let snap3 = root.join("snapshots").join(id3);
        assert!(!snap3.join("sqlite_app/db/test.db-shm").exists());

        // Verify recovered database has 'v2'
        let recovered_db = Connection::open(snap3.join("sqlite_app/db/test.db")).unwrap();
        let count: i64 = recovered_db
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn watch_queue_and_replication_failure_state_persisted_and_surfaced_in_status() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("backup");
        fs::create_dir_all(&root).unwrap();

        // 1. Test watch queue persistence
        let mut queue = load_watch_queue(&root).unwrap();
        assert!(queue.pending.is_empty());
        let now = Utc::now();
        queue.pending.insert(
            "codex/sessions".into(),
            WatchQueueEntry {
                app: "codex".into(),
                slot: "sessions".into(),
                source_path: PathBuf::from("C:/fake/path"),
                reasons: vec!["file modified".into()],
                first_event_at: now,
                last_event_at: now,
                attempts: 1,
                last_error: Some("capture timeout".into()),
            },
        );
        save_watch_queue(&root, &queue).unwrap();
        let loaded_queue = load_watch_queue(&root).unwrap();
        assert_eq!(loaded_queue, queue);

        // 2. Test replication failure state persistence
        let mut rep_state = load_replication_state(&root).unwrap();
        assert!(rep_state.failures.is_empty());
        rep_state.failures.insert(
            "snap-1".into(),
            ReplicationFailureEntry {
                target: "remote".into(),
                snapshot_id: "snap-1".into(),
                attempts: 2,
                last_error: "connection refused".into(),
                last_error_at: now,
                last_success_at: None,
            },
        );
        save_replication_state(&root, &rep_state).unwrap();
        let loaded_rep = load_replication_state(&root).unwrap();
        assert_eq!(loaded_rep, rep_state);

        // 3. Verify status() surfaces both watch_queue and replication_failures
        let st = status(&root).unwrap();
        assert_eq!(st["watch_queue"]["pending_count"], 1);
        assert_eq!(
            st["watch_queue"]["pending"]["codex/sessions"]["attempts"],
            1
        );
        assert_eq!(st["replication_failures"]["failure_count"], 1);
        assert_eq!(
            st["replication_failures"]["failures"]["snap-1"]["attempts"],
            2
        );
    }

    #[test]
    fn debounce_pure_timing_with_injected_instants() {
        let base = Instant::now();
        let last_capture = base;
        let first_event = Some(base + Duration::from_secs(1));
        let last_event = base + Duration::from_secs(1);

        // Event arrived 1s in, now at 3s (only 2s quiet, 2s elapsed since first) -> NOT due
        assert!(!capture_due_at(
            base + Duration::from_secs(3),
            last_capture,
            first_event,
            last_event
        ));

        // Now at 7s (6s quiet since last event at 1s) -> DUE (5s quiet merge)
        assert!(capture_due_at(
            base + Duration::from_secs(7),
            last_capture,
            first_event,
            last_event
        ));

        // Continuous events every 2 seconds (never quiet for 5s)
        let last_event_continuous = base + Duration::from_secs(59);
        // At 59s: not 5s quiet, first_event was at 1s, elapsed 58s (<60s) -> NOT due
        assert!(!capture_due_at(
            base + Duration::from_secs(59),
            last_capture,
            first_event,
            last_event_continuous
        ));

        // At 62s: first_event was at 1s, elapsed 61s (>=60s cap reached!) -> DUE (60s upper bound)
        assert!(capture_due_at(
            base + Duration::from_secs(62),
            last_capture,
            first_event,
            last_event_continuous
        ));

        // 5-minute fallback without any events:
        assert!(!capture_due_at(
            base + Duration::from_secs(299),
            last_capture,
            None,
            base
        ));
        assert!(capture_due_at(
            base + Duration::from_secs(300),
            last_capture,
            None,
            base
        ));
    }

    #[test]
    fn host_version_probe_has_hard_timeout_and_temp_home_never_reads_real_profile() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("isolated_home");
        fs::create_dir_all(&home).unwrap();

        // 1. With injected home, probing non-existent host never reads real profile and returns Unknown
        let (version, prov) = detect_host_version("cursor", &home.join("state.vscdb"), Some(&home));
        assert_eq!(version, None);
        assert_eq!(prov, VersionProvenance::Unknown);

        // 2. Put product.json inside isolated home
        let cursor_app = home.join("AppData/Local/Programs/cursor/resources/app");
        fs::create_dir_all(&cursor_app).unwrap();
        fs::write(cursor_app.join("product.json"), b"{\"version\":\"0.45.6\"}").unwrap();

        let (version, prov) = detect_host_version("cursor", &home.join("state.vscdb"), Some(&home));
        assert_eq!(version.as_deref(), Some("cursor 0.45.6"));
        assert_eq!(prov, VersionProvenance::Probed);

        // 3. Test command_version_timeout returns None without hanging
        let start = Instant::now();
        let res = command_version_timeout("nonexistent_binary_xyz_123", Duration::from_millis(50));
        assert_eq!(res, None);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn failed_capture_preserves_existing_snapshots_byte_identical() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("backup");
        let source_dir = temp.path().join("source");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("history.jsonl"), b"original content\n").unwrap();

        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "codex".into(),
                component: ComponentKind::Sessions,
                slot: "sessions".into(),
                path: source_dir.clone(),
                host_version: Some("fixture-v1".into()),
                ..Default::default()
            }],
            ..Default::default()
        };

        // 1. Successful initial capture
        let first = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        assert_eq!(first["status"], "captured");
        let snap_id = first["snapshot_id"].as_str().unwrap().to_string();
        let snap_dir = root.join("snapshots").join(&snap_id);
        let manifest_bytes = fs::read(snap_dir.join("manifest.json")).unwrap();
        let file_bytes = fs::read(snap_dir.join("codex/sessions/history.jsonl")).unwrap();

        // 2. Modify source to trigger next capture
        fs::write(source_dir.join("history.jsonl"), b"modified content\n").unwrap();

        // 3. Force write error during capture by blocking staging with a file
        let staging_root = root.join("staging");
        let _ = fs::remove_dir_all(&staging_root);
        fs::write(&staging_root, b"blocking-staging-directory").unwrap();

        // 4. Capture must fail and return an error (never "captured")
        let failed = capture_sources(&config, &root, &AppArgs::default(), None);
        assert!(
            failed.is_err(),
            "capture must return error on staging write failure"
        );

        // 5. Existing snapshots and manifests must remain 100% byte-identical
        assert_eq!(
            fs::read(snap_dir.join("manifest.json")).unwrap(),
            manifest_bytes,
            "manifest must remain byte-identical after failed capture"
        );
        assert_eq!(
            fs::read(snap_dir.join("codex/sessions/history.jsonl")).unwrap(),
            file_bytes,
            "captured source files must remain byte-identical after failed capture"
        );

        // 6. Confirm no pruning / history deletion on failure
        let manifests = list_manifests(&root).unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].snapshot_id, snap_id);
    }

    #[test]
    fn remote_offline_catch_up_records_failure_and_clears_on_reconnection() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("backup");
        let source_dir = temp.path().join("source");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("test.jsonl"), b"session data\n").unwrap();

        let password = temp.path().join("password");
        fs::write(&password, "synthetic-test-password").unwrap();
        let local_repo = temp.path().join("local-repo");
        let remote_repo = temp.path().join("remote-repo");

        let Some(restic) = test_restic() else {
            return;
        };

        // Initialize local repo only; remote_repo is deliberately left uninitialized (offline/unreachable)
        let _ = Command::new(&restic)
            .args(["-r"])
            .arg(&local_repo)
            .arg("init")
            .env("RESTIC_PASSWORD_FILE", &password)
            .status()
            .unwrap();

        let config = NativeConfig {
            data_root: Some(root.clone()),
            restic: Some(restic.clone()),
            password_file: Some(password.clone()),
            local_repository: Some(local_repo.display().to_string()),
            remote_repository: Some(remote_repo.display().to_string()),
            sources: vec![SourceConfig {
                app: "codex".into(),
                component: ComponentKind::Sessions,
                slot: "sessions".into(),
                path: source_dir,
                host_version: Some("fixture-v1".into()),
                ..Default::default()
            }],
            ..Default::default()
        };

        // 1. Capture snapshot locally
        let captured = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        let snap_id = captured["snapshot_id"].as_str().unwrap().to_string();

        // 2. replicate_pending with remote unreachable: records failure in persisted state
        let rep_res = replicate_pending(&config, &root, true);
        assert!(
            rep_res.is_err(),
            "replication to uninitialized remote must return error"
        );

        let rep_state = load_replication_state(&root).unwrap();
        assert_eq!(rep_state.failures.len(), 1);
        let fail_entry = rep_state.failures.get(&snap_id).unwrap();
        assert_eq!(fail_entry.target, "remote");
        assert_eq!(fail_entry.attempts, 1);

        // Local backup succeeded and is recorded in manifest, but remote is absent
        let manifest = load_manifest(&root.join("snapshots").join(&snap_id)).unwrap();
        assert!(manifest.local_restic_snapshot.is_some());
        assert!(manifest.remote_restic_snapshot.is_none());

        // Status surfaces replication failure
        let st = status(&root).unwrap();
        assert_eq!(st["replication_failures"]["failure_count"], 1);

        // 3. Now remote becomes reachable: initialize remote repo
        let _ = Command::new(&restic)
            .args(["-r"])
            .arg(&remote_repo)
            .arg("init")
            .env("RESTIC_PASSWORD_FILE", &password)
            .status()
            .unwrap();

        // 4. Next replicate_pending catches up: uploads pending snapshot and clears failure
        let catchup_res = replicate_pending(&config, &root, true);
        assert!(
            catchup_res.is_ok(),
            "catchup replication must succeed when remote is reachable"
        );

        let rep_state_after = load_replication_state(&root).unwrap();
        assert!(
            rep_state_after.failures.is_empty(),
            "failures must be cleared after catchup"
        );

        let manifest_after = load_manifest(&root.join("snapshots").join(&snap_id)).unwrap();
        assert!(
            manifest_after.remote_restic_snapshot.is_some(),
            "remote snapshot must now be recorded"
        );

        let st_after = status(&root).unwrap();
        assert_eq!(st_after["replication_failures"]["failure_count"], 0);
    }

    #[test]
    fn watch_restart_with_pending_changes_reconciles_immediately_on_startup() {
        // A. Timing decision: if pending change arrived before restart (>5s quiet or >60s elapsed),
        // capture_due_at returns true immediately upon restart (now == last_capture)
        let now = Instant::now();
        let pending_event = now.checked_sub(Duration::from_secs(10)).unwrap();
        assert!(capture_due_at(now, now, Some(pending_event), pending_event));

        // B. Functional catch-up: startup reconciliation immediately captures pending changes
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("backup");
        let source_dir = temp.path().join("source");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(
            source_dir.join("session.jsonl"),
            b"content before shutdown\n",
        )
        .unwrap();

        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "codex".into(),
                component: ComponentKind::Sessions,
                slot: "sessions".into(),
                path: source_dir.clone(),
                host_version: Some("fixture-v1".into()),
                ..Default::default()
            }],
            ..Default::default()
        };

        // Initial capture before shutdown
        let snap1 = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        let id1 = snap1["snapshot_id"].as_str().unwrap().to_string();

        // Changes occur while watch process is stopped / restarting
        fs::write(
            source_dir.join("session.jsonl"),
            b"offline change during restart\n",
        )
        .unwrap();

        // Startup reconciliation (exact path executed in watch_sources on startup)
        let restarted = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        assert_eq!(restarted["status"], "captured");
        let id2 = restarted["snapshot_id"].as_str().unwrap();
        assert_ne!(id1, id2);

        let snap2_dir = root.join("snapshots").join(id2);
        assert_eq!(
            fs::read(snap2_dir.join("codex/sessions/session.jsonl")).unwrap(),
            b"offline change during restart\n"
        );
    }

    #[test]
    fn test_native_pull_cli_parsing() {
        let parsed = NativeArgs::try_parse_from(["chronicle-native", "pull"]).unwrap();
        assert!(matches!(
            parsed.command,
            NativeCommand::Pull(PullArgs {
                snapshot_id: None,
                from: PullSource::Remote,
                host: None,
            })
        ));

        let parsed = NativeArgs::try_parse_from([
            "chronicle-native",
            "pull",
            "0afca009-7911-4ed8-8121-4601a64aa5bd",
            "--from",
            "local",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            NativeCommand::Pull(PullArgs {
                snapshot_id: Some(ref id),
                from: PullSource::Local,
                host: None,
            }) if id == "0afca009-7911-4ed8-8121-4601a64aa5bd"
        ));
    }

    #[test]
    fn test_native_pull_disaster_recovery_and_edge_cases() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source-root");
        fs::create_dir_all(&root).unwrap();
        let source_root = temp.path().join("source-sessions");
        fs::create_dir_all(&source_root).unwrap();
        let db_path = source_root.join("conversation.db");
        let connection = Connection::open(&db_path).unwrap();
        connection
            .execute("CREATE TABLE steps (value BLOB)", [])
            .unwrap();
        connection
            .execute("INSERT INTO steps VALUES (X'0102')", [])
            .unwrap();
        let source = SourceConfig {
            app: "fixture".into(),
            component: ComponentKind::Sessions,
            slot: "sessions".into(),
            path: source_root,
            host_version: Some("fixture".into()),
            ..Default::default()
        };
        let password = temp.path().join("password");
        fs::write(&password, "synthetic-test-password").unwrap();
        let local_repo = temp.path().join("local-repo");
        let remote_repo = temp.path().join("remote-repo");
        let Some(restic) = test_restic() else {
            return;
        };

        // Initialize local and remote repos
        for repo in [&local_repo, &remote_repo] {
            let status = Command::new(&restic)
                .args(["-r"])
                .arg(repo)
                .arg("init")
                .env("RESTIC_PASSWORD_FILE", &password)
                .status()
                .unwrap();
            assert!(status.success());
        }

        let config = NativeConfig {
            data_root: Some(root.clone()),
            restic: Some(restic.clone()),
            password_file: Some(password.clone()),
            local_repository: Some(local_repo.display().to_string()),
            remote_repository: Some(remote_repo.display().to_string()),
            sources: vec![source],
            ..NativeConfig::default()
        };

        // Capture snapshot 1 and replicate
        let captured1 = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        let snapshot_id1 = captured1["snapshot_id"].as_str().unwrap().to_string();
        let rep1 = replicate(&config, &root, Some(&snapshot_id1)).unwrap();
        assert!(rep1["local_restic_snapshot"].is_string());
        assert!(rep1["remote_restic_snapshot"].is_string());

        // Capture snapshot 2 with newer timestamp and replicate
        std::thread::sleep(Duration::from_millis(1100));
        connection
            .execute("INSERT INTO steps VALUES (X'0304')", [])
            .unwrap();
        let captured2 = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        let snapshot_id2 = captured2["snapshot_id"].as_str().unwrap().to_string();
        assert_ne!(snapshot_id1, snapshot_id2);
        let rep2 = replicate(&config, &root, Some(&snapshot_id2)).unwrap();
        assert!(rep2["remote_restic_snapshot"].is_string());

        // 1. End-to-end disaster recovery:
        // Fresh empty root, config only points to remote repository
        let disaster_root = temp.path().join("disaster-recovery-root");
        fs::create_dir_all(&disaster_root).unwrap();
        let disaster_config = NativeConfig {
            data_root: Some(disaster_root.clone()),
            restic: Some(restic.clone()),
            password_file: Some(password.clone()),
            remote_repository: Some(remote_repo.display().to_string()),
            local_repository: None,
            ..NativeConfig::default()
        };

        // When no snapshot ID is given, pull latest (snapshot_id2)
        let pulled_latest = pull_snapshot(
            &disaster_config,
            &disaster_root,
            PullArgs {
                snapshot_id: None,
                from: PullSource::Remote,
                host: None,
            },
        )
        .unwrap();
        assert_eq!(pulled_latest["mode"], "pulled");
        assert_eq!(pulled_latest["snapshot_id"], snapshot_id2);
        assert_eq!(pulled_latest["from"], "remote");
        assert!(pulled_latest["restic_snapshot"].is_string());
        assert_eq!(pulled_latest["files"].as_u64().unwrap(), 1);
        assert_eq!(pulled_latest["next"], "restore <id> --target <dir>");

        // Verify local on the pulled snapshot passes
        let manifest2 = verify_local(&disaster_root, &snapshot_id2).unwrap();
        for file in &manifest2.files {
            let orig_file = root
                .join("snapshots")
                .join(&snapshot_id2)
                .join(&file.relative);
            let pulled_file = disaster_root
                .join("snapshots")
                .join(&snapshot_id2)
                .join(&file.relative);
            assert_eq!(
                hash_file(&orig_file).unwrap(),
                hash_file(&pulled_file).unwrap()
            );
        }

        // Verify restore --target extracts successfully
        let extracted = temp.path().join("extracted-from-disaster");
        let restore_args = RestoreArgs {
            snapshot_id: Some(snapshot_id2.clone()),
            target: Some(extracted.clone()),
            dry_run: false,
            apply_plan: None,
        };
        restore_snapshot(&disaster_root, restore_args).unwrap();
        let restored_db = extracted.join("fixture/sessions/conversation.db");
        assert!(restored_db.is_file());
        let conn = Connection::open(&restored_db).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM steps", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);

        // 2. Duplicate pull returns already-present without downloading from restic
        let staging_root = disaster_root.join("staging");
        if staging_root.exists() {
            let left_over: Vec<_> = fs::read_dir(&staging_root)
                .unwrap()
                .filter_map(Result::ok)
                .collect();
            assert!(
                left_over.is_empty(),
                "staging should be clean before duplicate pull"
            );
        }

        let dup_pull = pull_snapshot(
            &disaster_config,
            &disaster_root,
            PullArgs {
                snapshot_id: Some(snapshot_id2.clone()),
                from: PullSource::Remote,
                host: None,
            },
        )
        .unwrap();
        assert_eq!(dup_pull["mode"], "already-present");
        assert_eq!(dup_pull["snapshot_id"], snapshot_id2);
        assert_eq!(dup_pull["files"].as_u64().unwrap(), 1);

        // Assert that staging was never touched/created during duplicate pull
        if staging_root.exists() {
            let entries: Vec<_> = fs::read_dir(&staging_root)
                .unwrap()
                .filter_map(Result::ok)
                .collect();
            assert!(
                entries.is_empty(),
                "staging must remain completely empty (no restic restore executed): {:?}",
                entries
            );
        }

        // 3. Pull non-existent snapshot id returns clear error listing available ids
        let fake_id = uuid::Uuid::new_v4().to_string();
        let err_pull = pull_snapshot(
            &disaster_config,
            &disaster_root,
            PullArgs {
                snapshot_id: Some(fake_id.clone()),
                from: PullSource::Remote,
                host: None,
            },
        );
        assert!(err_pull.is_err());
        let err_msg = err_pull.unwrap_err().to_string();
        assert!(err_msg.contains(&fake_id));
        assert!(err_msg.contains(&snapshot_id1) || err_msg.contains(&snapshot_id2));

        // 4. Local snapshot exists but tampered -> rejected and not overwritten without downloading
        let tampered_file = disaster_root
            .join("snapshots")
            .join(&snapshot_id2)
            .join("fixture/sessions/conversation.db");
        fs::write(&tampered_file, b"corrupted-tampered-bytes").unwrap();

        let tampered_pull = pull_snapshot(
            &disaster_config,
            &disaster_root,
            PullArgs {
                snapshot_id: Some(snapshot_id2.clone()),
                from: PullSource::Remote,
                host: None,
            },
        );
        assert!(tampered_pull.is_err());
        let tampered_msg = tampered_pull.unwrap_err().to_string();
        assert!(tampered_msg.contains("failed verification"));
        assert_eq!(
            fs::read(&tampered_file).unwrap(),
            b"corrupted-tampered-bytes"
        );
        if staging_root.exists() {
            let entries: Vec<_> = fs::read_dir(&staging_root)
                .unwrap()
                .filter_map(Result::ok)
                .collect();
            assert!(
                entries.is_empty(),
                "staging must not have been created on tampered pull rejection: {:?}",
                entries
            );
        }

        // 5. Pull older snapshot (snapshot_id1) explicitly
        let pulled_older = pull_snapshot(
            &disaster_config,
            &disaster_root,
            PullArgs {
                snapshot_id: Some(snapshot_id1.clone()),
                from: PullSource::Remote,
                host: None,
            },
        )
        .unwrap();
        assert_eq!(pulled_older["mode"], "pulled");
        assert_eq!(pulled_older["snapshot_id"], snapshot_id1);
        let manifest1 = verify_local(&disaster_root, &snapshot_id1).unwrap();
        assert_eq!(manifest1.snapshot_id, snapshot_id1);

        // 6. Pull from local repository using --from local
        let local_pull_root = temp.path().join("local-pull-root");
        fs::create_dir_all(&local_pull_root).unwrap();
        let local_pull_config = NativeConfig {
            data_root: Some(local_pull_root.clone()),
            restic: Some(restic.clone()),
            password_file: Some(password.clone()),
            local_repository: Some(local_repo.display().to_string()),
            remote_repository: None,
            ..NativeConfig::default()
        };
        let pulled_local = pull_snapshot(
            &local_pull_config,
            &local_pull_root,
            PullArgs {
                snapshot_id: Some(snapshot_id1.clone()),
                from: PullSource::Local,
                host: None,
            },
        )
        .unwrap();
        assert_eq!(pulled_local["mode"], "pulled");
        assert_eq!(pulled_local["from"], "local");

        // 7. Missing config errors
        let no_restic_config = NativeConfig::default();
        let err =
            pull_snapshot(&no_restic_config, &disaster_root, PullArgs::default()).unwrap_err();
        assert!(
            err.to_string()
                .contains("restic executable is not configured")
        );
    }
}
