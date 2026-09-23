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
mod codex;
mod cursor;
mod filehosts;
pub mod hosts;
pub use capture::*;
pub mod components;
pub use components::*;
pub use hosts::{
    HostCheckItem, HostCheckStatus, HostRecordStatus, HostsArgs, HostsCheckResult, HostsRecordArgs,
    HostsRevokeArgs, HostsSubcommand, VerifiedHostRecord, VerifiedHostsRegistry, load_hosts_check,
};

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
    Hosts(hosts::HostsArgs),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct InstallPlan {
    kind: InstallPlanKind,
    version: u32,
    snapshot_id: String,
    app: String,
    host_version: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    drill: bool,
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
pub(crate) struct InstallAction {
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
struct JournalActionRecord {
    index: usize,
    #[serde(rename = "kind")]
    action_kind: InstallActionKind,
    target: PathBuf,
    target_existed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup_file: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor_mutations: Option<cursor::CursorMutations>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct InstallJournal {
    snapshot_id: String,
    #[serde(default)]
    app: String,
    phase: String,
    #[serde(default)]
    target_fingerprint_before: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_fingerprint_after: Option<String>,
    completed: Vec<String>,
    #[serde(default)]
    mutations: Vec<JournalActionRecord>,
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
        NativeCommand::Hosts(hosts_args) => match hosts_args.command {
            hosts::HostsSubcommand::Record(record_args) => {
                hosts::handle_hosts_record(&root, record_args)?
            }
            hosts::HostsSubcommand::Revoke(revoke_args) => {
                hosts::handle_hosts_revoke(&root, revoke_args)?
            }
            hosts::HostsSubcommand::Check => {
                hosts::handle_hosts_check(&config, &root, args.home.as_deref())?
            }
        },
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

fn restore_snapshot(
    root: &Path,
    args: RestoreArgs,
    home: Option<&Path>,
) -> Result<serde_json::Value> {
    if let Some(plan_path) = args.apply_plan {
        if args.dry_run {
            let raw: serde_json::Value = serde_json::from_str(&fs::read_to_string(&plan_path)?)?;
            return Ok(serde_json::json!({"mode": "apply-dry-run", "plan": raw}));
        }
        return apply_saved_plan_with_home(root, &plan_path, args.force, home);
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

fn normalize_path_str(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

fn may_contain_credentials(target: &Path, kind: InstallActionKind) -> bool {
    kind == InstallActionKind::CursorSessionMerge
        || credential_path(target)
        || target
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name == "state.vscdb" || name == "state.vscdb-wal")
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
    let snapshot_dir = root.join("snapshots").join(&manifest.snapshot_id);
    ensure!(
        snapshot_dir.is_dir(),
        "snapshot directory missing: {}",
        snapshot_dir.display()
    );
    let mut actions = Vec::new();
    let mut path_map = BTreeMap::new();
    for mapping in mappings {
        if let Some((from, to)) = mapping.split_once('=') {
            path_map.insert(from.to_string(), PathBuf::from(to));
        }
    }
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
            let snapshot_file = snapshot_dir.join(&file.relative);
            ensure!(
                snapshot_file.is_file(),
                "snapshot file missing: {}",
                file.relative
            );
            let relative = &file.relative[prefix.len()..];
            ensure!(
                safe_relative(relative) || relative.is_empty(),
                "unsafe relative path: {relative}"
            );
            ensure!(
                filehosts::is_unsupported_path(app, relative).is_none(),
                "unsupported item in snapshot: {relative}"
            );
            let source_path = if source.path.file_name().and_then(|n| n.to_str()) == Some(relative)
            {
                source.path.clone()
            } else {
                source.path.join(relative)
            };
            let same_location = normalize_path_str(&target_source.path.to_string_lossy())
                == normalize_path_str(&source.path.to_string_lossy());
            let target_path = if let Some(cwd_seg) =
                filehosts::encoded_cwd_segment(app, &source.slot, relative)
            {
                if same_location {
                    if target_source.path.extension().is_some() {
                        target_source.path.clone()
                    } else {
                        target_source.path.join(relative)
                    }
                } else {
                    let mapped_seg = mappings
                        .iter()
                        .find_map(|m| {
                            m.split_once('=').and_then(|(from, to)| {
                                if from == cwd_seg {
                                    Some(to)
                                } else {
                                    None
                                }
                            })
                        })
                        .with_context(|| {
                            format!(
                                "blind cross-device path copy refused: path contains encoded cwd segment '{cwd_seg}' without explicit mapping"
                            )
                        })?;
                    let rest = &relative[cwd_seg.len()..];
                    let rest_trimmed = rest.trim_start_matches(['/', '\\']);
                    let mapped_rel = if rest_trimmed.is_empty() {
                        PathBuf::from(mapped_seg)
                    } else {
                        Path::new(mapped_seg).join(rest_trimmed)
                    };
                    if target_source.path.extension().is_some() {
                        target_source.path.clone()
                    } else {
                        target_source.path.join(mapped_rel)
                    }
                }
            } else if target_source.path.extension().is_some() {
                target_source.path.clone()
            } else {
                target_source.path.join(relative)
            };
            ensure!(
                !target_path
                    .components()
                    .any(|c| matches!(c, Component::ParentDir)),
                "target path contains directory traversal: {}",
                target_path.display()
            );
            let target_base = if target_source.path.extension().is_some() {
                target_source.path.parent().unwrap_or(&target_source.path)
            } else {
                &target_source.path
            };
            ensure!(
                target_path.starts_with(target_base),
                "target path escapes destination root: {}",
                target_path.display()
            );
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
        !host_version.is_empty() && host_version != "unknown",
        "unknown host version: native installation is refused"
    );
    let is_verified = verified_install_host(root, app, &host_version);
    let drill = if !is_verified {
        hosts::validate_drill_targets(&actions, home).map_err(|e| {
            anyhow::anyhow!(
                "native installation has not been verified for this host version ({e}); isolate files with --target and complete host acceptance first"
            )
        })?;
        true
    } else {
        false
    };
    Ok(InstallPlan {
        kind: InstallPlanKind::Install,
        version: VERSION,
        snapshot_id: manifest.snapshot_id.clone(),
        app: app.into(),
        host_version,
        drill,
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
    let source_norm = normalize_path_str(&source.path.to_string_lossy());
    for mapping in mappings {
        let (from, to) = mapping
            .split_once('=')
            .context("mapping must be SOURCE=TARGET")?;
        let from_norm = normalize_path_str(from);
        if from_norm == source_norm
            || from.eq_ignore_ascii_case(&source.slot)
            || from_norm
                == format!(
                    "{}/{}",
                    source.app.to_lowercase(),
                    source.slot.to_lowercase()
                )
        {
            return Ok(SourceConfig {
                path: PathBuf::from(to),
                ..source.clone()
            });
        }
        if let (Ok(from_canon), Ok(source_canon)) =
            (Path::new(from).canonicalize(), source.path.canonicalize())
            && from_canon == source_canon
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
        validate_store(&action.target)?;
        if action.target.exists() {
            files.insert(
                action.target.display().to_string(),
                Some(hash_file(&action.target)?),
            );
        } else {
            files.insert(action.target.display().to_string(), None);
        }
        let wal = PathBuf::from(format!("{}-wal", action.target.display()));
        if wal.exists() {
            validate_store(&wal)?;
            files.insert(wal.display().to_string(), Some(hash_file(&wal)?));
        }
    }
    Ok(hex(&Sha256::digest(serde_json::to_vec(&files)?)))
}

#[cfg(test)]
pub(crate) fn apply_saved_plan(root: &Path, path: &Path, force: bool) -> Result<serde_json::Value> {
    apply_saved_plan_with_home(root, path, force, None)
}

fn apply_saved_plan_with_home(
    root: &Path,
    path: &Path,
    force: bool,
    home: Option<&Path>,
) -> Result<serde_json::Value> {
    let raw: serde_json::Value = serde_json::from_str(&fs::read_to_string(path)?)?;
    if raw.get("kind").and_then(serde_json::Value::as_str) == Some("install") {
        let plan: InstallPlan = serde_json::from_value(raw)?;
        return apply_install_plan_with_home(root, plan, force, home);
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

fn rollback_journal(journal: &InstallJournal) -> Result<()> {
    for rec in journal.mutations.iter().rev() {
        if rec.action_kind == InstallActionKind::CursorSessionMerge {
            if rec.target_existed {
                if let Some(cm) = &rec.cursor_mutations {
                    cursor::rollback_cursor_mutations(&rec.target, cm)?;
                }
            } else {
                let _ = fs::remove_file(&rec.target);
                for suffix in ["-wal", "-shm", "-journal"] {
                    let _ =
                        fs::remove_file(PathBuf::from(format!("{}{suffix}", rec.target.display())));
                }
            }
        } else if rec.target_existed {
            if let Some(backup) = &rec.backup_file {
                for suffix in ["-wal", "-shm", "-journal"] {
                    let _ =
                        fs::remove_file(PathBuf::from(format!("{}{suffix}", rec.target.display())));
                }
                fs::copy(backup, &rec.target)?;
            }
        } else {
            let _ = fs::remove_file(&rec.target);
            for suffix in ["-wal", "-shm", "-journal"] {
                let _ = fs::remove_file(PathBuf::from(format!("{}{suffix}", rec.target.display())));
            }
        }
    }
    Ok(())
}

fn recover_interrupted_installs(root: &Path, app: &str) -> Result<Vec<String>> {
    let rollback_root = root.join("rollback");
    if !rollback_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut recovered = Vec::new();
    let mut entries: Vec<_> =
        fs::read_dir(&rollback_root)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let journal_path = entry.path().join("journal.json");
        if journal_path.is_file() {
            let content = fs::read_to_string(&journal_path)?;
            let mut journal: InstallJournal = serde_json::from_str(&content)?;
            if journal.app == app && journal.phase == "applying" {
                rollback_journal(&journal)?;
                journal.phase = "rolled-back-after-interruption".into();
                atomic_json(&journal_path, &journal)?;
                recovered.push(journal.snapshot_id);
            }
        }
    }
    Ok(recovered)
}

#[cfg(test)]
pub(crate) fn apply_install_plan(
    root: &Path,
    plan: InstallPlan,
    force: bool,
) -> Result<serde_json::Value> {
    apply_install_plan_with_home(root, plan, force, None)
}

fn apply_install_plan_with_home(
    root: &Path,
    plan: InstallPlan,
    force: bool,
    home: Option<&Path>,
) -> Result<serde_json::Value> {
    let manifest = verify_local(root, &plan.snapshot_id)?;
    ensure!(
        !plan.host_version.is_empty() && plan.host_version != "unknown",
        "unknown host version: native installation is refused"
    );
    let is_verified = verified_install_host(root, &plan.app, &plan.host_version);
    if !is_verified {
        ensure!(
            plan.drill,
            "native installation has not been verified for this host version; isolate files with --target and complete host acceptance first"
        );
        hosts::validate_drill_targets(&plan.actions, home).map_err(|e| {
            anyhow::anyhow!(
                "native installation has not been verified for this host version ({e}); isolate files with --target and complete host acceptance first"
            )
        })?;
    }
    ensure!(
        plan.version == VERSION && plan.kind == InstallPlanKind::Install,
        "unsupported install plan"
    );
    ensure_host_stopped(&plan.app, force)?;
    let recovered_interrupted = recover_interrupted_installs(root, &plan.app)?;
    ensure!(
        plan.target_fingerprint == install_fingerprint(&plan.actions)?,
        "restore target changed; generate a new plan"
    );
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
        let existed = action.target.exists();
        let backup = if existed {
            if may_contain_credentials(&action.target, action.kind) {
                None
            } else if sqlite_path(&action.target) {
                let backup_file = plan.rollback_dir.join(format!("{index:04}.backup"));
                backup_sqlite(&action.target, &backup_file)?;
                Some(backup_file)
            } else {
                let backup_file = plan.rollback_dir.join(format!("{index:04}.backup"));
                fs::copy(&action.target, &backup_file)?;
                Some(backup_file)
            }
        } else {
            None
        };
        prepared.push((action, existed, backup));
    }
    let journal_path = plan.rollback_dir.join("journal.json");
    let mut journal = InstallJournal {
        snapshot_id: plan.snapshot_id.clone(),
        app: plan.app.clone(),
        phase: "applying".into(),
        target_fingerprint_before: plan.target_fingerprint.clone(),
        target_fingerprint_after: None,
        completed: Vec::new(),
        mutations: Vec::new(),
    };
    atomic_json(&journal_path, &journal)?;
    let mut codex_unregistered_threads = Vec::new();
    let result = (|| -> Result<()> {
        for (index, action) in plan.actions.iter().enumerate() {
            journal.mutations.push(JournalActionRecord {
                index,
                action_kind: action.kind,
                target: action.target.clone(),
                target_existed: prepared[index].1,
                backup_file: prepared[index].2.clone(),
                cursor_mutations: None,
            });
            atomic_json(&journal_path, &journal)?;
            let (cursor_mutations, skipped) = apply_install_action(root, &plan, action)?;
            codex_unregistered_threads.extend(skipped);
            if let Some(record) = journal.mutations.last_mut() {
                record.cursor_mutations = cursor_mutations;
            }
            journal.completed.push(action.target.display().to_string());
            atomic_json(&journal_path, &journal)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        rollback_journal(&journal)?;
        journal.phase = "rolled-back-after-error".into();
        atomic_json(&journal_path, &journal)?;
        return Err(error);
    }
    journal.phase = "completed".into();
    journal.target_fingerprint_after = Some(install_fingerprint(&plan.actions)?);
    atomic_json(&journal_path, &journal)?;
    Ok(serde_json::json!({
        "mode": "installed-files-and-indexes",
        "snapshot_id": plan.snapshot_id,
        "app": plan.app,
        "actions": plan.actions.len(),
        "rollback": plan.rollback_dir,
        "recovered_interrupted": recovered_interrupted,
        "file_verification": "passed",
        "client_open": "not-verified",
        "client_restart": "not-verified",
        "continuation": "not-verified",
        "codex_unregistered_threads": codex_unregistered_threads,
    }))
}

fn verified_install_host(root: &Path, app: &str, version: &str) -> bool {
    hosts::verified_install_host(root, app, version)
}

fn apply_install_action(
    root: &Path,
    plan: &InstallPlan,
    action: &InstallAction,
) -> Result<(
    Option<cursor::CursorMutations>,
    Vec<codex::SkippedCodexThread>,
)> {
    let source = root
        .join("snapshots")
        .join(&plan.snapshot_id)
        .join(&action.source);
    match action.kind {
        InstallActionKind::Copy | InstallActionKind::CopyIfMissing => {
            if action.target.exists() {
                if action.kind == InstallActionKind::CopyIfMissing {
                    return Ok((None, Vec::new()));
                }
                let source_hash = hash_file(&source)?;
                ensure!(
                    source_hash == hash_file(&action.target)?,
                    "restore conflict: {}",
                    action.target.display()
                );
                return Ok((None, Vec::new()));
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
            Ok((None, Vec::new()))
        }
        InstallActionKind::CursorSessionMerge => {
            if let Some(parent) = action.target.parent() {
                fs::create_dir_all(parent)?;
            }
            let (_copied, muts) = cursor::merge_sessions_strictly(&source, &action.target)?;
            Ok((Some(muts), Vec::new()))
        }
        InstallActionKind::CodexIndexMerge => {
            let skipped = codex::merge_index(&source, &action.target, &plan.path_map)?;
            Ok((None, skipped))
        }
        InstallActionKind::SessionIndexMerge => {
            let registered = plan
                .actions
                .iter()
                .find(|action| action.kind == InstallActionKind::CodexIndexMerge)
                .map(|action| action.target.as_path());
            merge_session_index(&source, &action.target, registered)?;
            Ok((None, Vec::new()))
        }
    }
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
    let watch_queue = load_watch_queue(root).unwrap_or_default();
    let rep_state = load_replication_state(root).unwrap_or_default();
    let hosts_check = hosts::load_hosts_check(root);
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
        "hosts_check": hosts_check,
    }))
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
                ..Default::default()
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
            apply_install_plan(&root, plan.clone(), true).unwrap()["file_verification"],
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
        drop(target_db);

        // 1. Verify rollback directory NEVER backs up the whole target SQLite DB or leaks credentials
        assert!(
            !plan.rollback_dir.join("0000.backup").exists(),
            "whole target SQLite DB must NOT be backed up"
        );
        for entry in fs::read_dir(&plan.rollback_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "backup") {
                let content = fs::read_to_string(&path).unwrap_or_default();
                assert!(
                    !content.contains("FAKE_TARGET_AUTH"),
                    "credential leaked to rollback directory!"
                );
            }
        }
        let journal_content = fs::read_to_string(plan.rollback_dir.join("journal.json")).unwrap();
        assert!(
            !journal_content.contains("FAKE_TARGET_AUTH"),
            "credential leaked to journal.json!"
        );
        let journal: InstallJournal = serde_json::from_str(&journal_content).unwrap();
        assert_eq!(journal.phase, "completed");
        assert_eq!(journal.mutations.len(), 1);
        assert_eq!(
            journal.mutations[0]
                .cursor_mutations
                .as_ref()
                .unwrap()
                .inserted_item_table,
            vec!["composerData:session-1".to_string()]
        );

        // 2. Idempotent re-run with identical content allows successfully
        let idempotent_plan =
            build_install_plan(&manifest, "cursor", Some(&home), &[], &root).unwrap();
        assert_eq!(
            apply_install_plan(&root, idempotent_plan, true).unwrap()["file_verification"],
            "passed"
        );

        // 3. Conflict rejection: same ID with different content strictly rejected
        let target_db = Connection::open(&target).unwrap();
        target_db
            .execute(
                "UPDATE ItemTable SET value='conflict-edit' WHERE key='composerData:session-1'",
                [],
            )
            .unwrap();
        drop(target_db);
        let conflict_plan =
            build_install_plan(&manifest, "cursor", Some(&home), &[], &root).unwrap();
        let conflict_err = apply_install_plan(&root, conflict_plan, true).unwrap_err();
        assert!(
            conflict_err.to_string().contains("conflict"),
            "expected conflict error, got: {conflict_err}"
        );
        // Verify conflicting value was untouched
        let target_db = Connection::open(&target).unwrap();
        assert_eq!(
            target_db
                .query_row(
                    "SELECT value FROM ItemTable WHERE key='composerData:session-1'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "conflict-edit"
        );
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
        drop(target_db);

        // 4. Rollback on subsequent failure: clean rollback of Cursor mutations while preserving auth
        // Reset target to clean state with only auth
        let target_db = Connection::open(&target).unwrap();
        target_db
            .execute(
                "DELETE FROM ItemTable WHERE key='composerData:session-1'",
                [],
            )
            .unwrap();
        target_db
            .execute(
                "DELETE FROM composerHeaders WHERE composerId='session-1'",
                [],
            )
            .unwrap();
        drop(target_db);

        let mut failing_plan =
            build_install_plan(&manifest, "cursor", Some(&home), &[], &root).unwrap();
        // Add a second action that is guaranteed to fail
        failing_plan.actions.push(InstallAction {
            kind: InstallActionKind::Copy,
            source: "non_existent_source.json".into(),
            target: home.join("failing_target.json"),
        });
        failing_plan.target_fingerprint = install_fingerprint(&failing_plan.actions).unwrap();
        let fail_res = apply_install_plan(&root, failing_plan.clone(), true);
        assert!(fail_res.is_err());
        // Verify Cursor mutations were cleanly rolled back!
        let target_db = Connection::open(&target).unwrap();
        let session_key_exists: bool = target_db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM ItemTable WHERE key='composerData:session-1')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !session_key_exists,
            "session key should have been rolled back"
        );
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
        let journal_after_fail: InstallJournal = serde_json::from_str(
            &fs::read_to_string(failing_plan.rollback_dir.join("journal.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(journal_after_fail.phase, "rolled-back-after-error");
    }

    #[test]
    fn cursor_capture_allows_orphan_bubble_and_install_refuses_preserving_target() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let source_root = home.join("source");
        fs::create_dir_all(&source_root).unwrap();
        let source_db = source_root.join("state.vscdb");
        let source = Connection::open(&source_db).unwrap();
        source
            .execute_batch(
                "CREATE TABLE ItemTable(key TEXT PRIMARY KEY,value BLOB);
                 INSERT INTO ItemTable VALUES('bubbleId:orphan-comp:b1', '{\"text\":\"orphan\"}');",
            )
            .unwrap();
        drop(source);

        let root = temp.path().join("backup");
        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "cursor".into(),
                component: ComponentKind::Sessions,
                slot: "global-storage".into(),
                path: source_db,
                host_version: Some("fixture-v1".into()),
                ..Default::default()
            }],
            ..Default::default()
        };

        let captured = capture_sources(&config, &root, &AppArgs::default(), Some(&home)).unwrap();
        assert_eq!(captured["status"], "captured");
        let id = captured["snapshot_id"].as_str().unwrap();
        let manifest = verify_local(&root, id).unwrap();

        // Capture-level exclusions contain the integrity note for (a)
        assert!(manifest.exclusions.iter().any(|e| {
            e.contains("cursor/")
                && e.contains("state.vscdb: integrity:")
                && e.contains("missing dependency")
                && e.contains("orphan bubble")
        }));

        let target = home.join("AppData/Roaming/Cursor/User/globalStorage/state.vscdb");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        let target_db = Connection::open(&target).unwrap();
        target_db
            .execute_batch(
                "CREATE TABLE ItemTable(key TEXT PRIMARY KEY,value BLOB);
                 INSERT INTO ItemTable VALUES('cursor.accessToken','FAKE_TARGET_AUTH');",
            )
            .unwrap();
        drop(target_db);

        let target_bytes_before = fs::read(&target).unwrap();
        let plan = build_install_plan(&manifest, "cursor", Some(&home), &[], &root).unwrap();
        let err = apply_install_plan(&root, plan, true).unwrap_err();
        assert!(
            err.to_string().contains("missing dependency"),
            "expected missing dependency error, got: {err}"
        );

        // Target DB is byte-identical afterwards (including the fake auth key)
        let target_bytes_after = fs::read(&target).unwrap();
        assert_eq!(target_bytes_before, target_bytes_after);

        let target_db = Connection::open(&target).unwrap();
        let auth: String = target_db
            .query_row(
                "SELECT value FROM ItemTable WHERE key='cursor.accessToken'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(auth, "FAKE_TARGET_AUTH");
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
        state_db
            .execute(
                "INSERT INTO thread_dynamic_tools VALUES('ghost', 0, 'bash')",
                [],
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
                ..Default::default()
            },
            SourceConfig {
                app: "codex".into(),
                component: ComponentKind::Sessions,
                slot: "state".into(),
                path: source_state,
                host_version: Some("fixture-v1".into()),
                ..Default::default()
            },
            SourceConfig {
                app: "codex".into(),
                component: ComponentKind::Sessions,
                slot: "thread-history".into(),
                path: source_history,
                host_version: Some("fixture-v1".into()),
                ..Default::default()
            },
            SourceConfig {
                app: "codex".into(),
                component: ComponentKind::Sessions,
                slot: "index".into(),
                path: source_index,
                host_version: Some("fixture-v1".into()),
                ..Default::default()
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
        let install_result = apply_install_plan(&root, plan, true).unwrap();
        assert_eq!(install_result["file_verification"], "passed");
        assert_eq!(
            install_result["codex_unregistered_threads"],
            serde_json::json!([
                {
                    "id": "ghost",
                    "reason": "rollout not in snapshot"
                }
            ])
        );
        let target_db = Connection::open(&target_state).unwrap();
        assert_eq!(
            target_db
                .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            target_db
                .query_row(
                    "SELECT COUNT(*) FROM thread_dynamic_tools WHERE thread_id='ghost'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
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
                    ..Default::default()
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
    fn plugins_component_excluded_from_install_plan() {
        let temp = tempfile::tempdir().unwrap();
        let plugins_dir = temp.path().join("plugins");
        fs::create_dir_all(&plugins_dir).unwrap();
        fs::write(plugins_dir.join("plugin.json"), b"{\"enabled\":false}").unwrap();

        let root = temp.path().join("backup");
        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "claude-code".into(),
                component: ComponentKind::Plugins,
                slot: "plugins".into(),
                path: plugins_dir,
                host_version: Some("fixture-v1".into()),
                ..Default::default()
            }],
            components: vec![ComponentKind::Plugins],
            ..Default::default()
        };

        let captured = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        let manifest = verify_local(&root, captured["snapshot_id"].as_str().unwrap()).unwrap();
        assert_eq!(manifest.files.len(), 1);

        // build_install_plan only installs ComponentKind::Sessions, so plugins must produce no installable files
        let home = temp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let plan_err =
            build_install_plan(&manifest, "claude-code", Some(&home), &[], &root).unwrap_err();
        assert!(
            plan_err
                .to_string()
                .contains("no installable files for claude-code")
        );
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

    #[test]
    fn install_plan_is_self_contained_after_source_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let source_dir = home.join("source/codex");
        fs::create_dir_all(source_dir.join("sessions")).unwrap();
        fs::write(
            source_dir.join("sessions/test.jsonl"),
            b"{\"type\":\"session\"}\n",
        )
        .unwrap();
        let root = temp.path().join("backup");
        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "codex".into(),
                component: ComponentKind::Sessions,
                slot: "sessions".into(),
                path: source_dir.join("sessions"),
                host_version: Some("fixture-v1".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let captured = capture_sources(&config, &root, &AppArgs::default(), Some(&home)).unwrap();
        let id = captured["snapshot_id"].as_str().unwrap();

        // DELETE the original source files completely
        fs::remove_dir_all(&source_dir).unwrap();
        assert!(!source_dir.exists());

        // Verify build_install_plan works directly from snapshot directory
        let manifest = verify_local(&root, id).unwrap();
        let target_dir = home.join("target/codex/sessions");
        let plan = build_install_plan(
            &manifest,
            "codex",
            Some(&home),
            &[format!("sessions={}", target_dir.display())],
            &root,
        )
        .unwrap();
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].target, target_dir.join("test.jsonl"));
        let res = apply_install_plan(&root, plan, true).unwrap();
        assert_eq!(res["file_verification"], "passed");
        assert!(target_dir.join("test.jsonl").is_file());
    }

    #[test]
    fn host_version_gate_fail_closed_even_with_force() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let source_file = home.join("source/test.jsonl");
        fs::create_dir_all(source_file.parent().unwrap()).unwrap();
        fs::write(&source_file, b"content").unwrap();
        let root = temp.path().join("backup");
        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "claude-code".into(),
                component: ComponentKind::Sessions,
                slot: "projects".into(),
                path: source_file,
                host_version: Some("fixture-v1".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let captured = capture_sources(&config, &root, &AppArgs::default(), Some(&home)).unwrap();
        let id = captured["snapshot_id"].as_str().unwrap();
        let manifest = verify_local(&root, id).unwrap();

        // 1. Build plan fails if host_version is unverified
        let mut unverified_manifest = manifest.clone();
        unverified_manifest.sources[0].host_version = Some("unverified-future-v99".into());
        let err = build_install_plan(&unverified_manifest, "claude-code", Some(&home), &[], &root)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("native installation has not been verified")
        );

        // 2. Even if a forged plan bypasses build_install_plan, apply_install_plan MUST fail closed with force=true
        let mut plan =
            build_install_plan(&manifest, "claude-code", Some(&home), &[], &root).unwrap();
        plan.host_version = "unverified-future-v99".into();
        let apply_err = apply_install_plan(&root, plan, true).unwrap_err();
        assert!(
            apply_err
                .to_string()
                .contains("native installation has not been verified")
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

        // Test restore with into + dry_run
        let restore_args = RestoreArgs {
            snapshot_id: Some(id.to_string()),
            target: None,
            into: Some("grok".to_string()),
            dry_run: true,
            apply_plan: None,
            map: vec![],
            force: false,
        };
        let res = restore_snapshot(&root, restore_args, Some(&home)).unwrap();
        assert_eq!(res["mode"], "install-dry-run");
        // Verify ZERO writes made to target_dir or plans
        assert!(
            !target_dir.exists(),
            "dry run must make ZERO writes to target directory"
        );
        assert!(
            !root.join("plans").exists(),
            "dry run must not write plan files"
        );

        // Test extract with target + dry_run
        let extract_args = RestoreArgs {
            snapshot_id: Some(id.to_string()),
            target: Some(target_dir.clone()),
            into: None,
            dry_run: true,
            apply_plan: None,
            map: vec![],
            force: false,
        };
        let res2 = restore_snapshot(&root, extract_args, Some(&home)).unwrap();
        assert_eq!(res2["mode"], "extract-dry-run");
        assert!(
            !target_dir.exists(),
            "extract dry run must make ZERO writes"
        );
    }

    #[test]
    fn install_path_mapping_prevents_directory_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let source_file = home.join("source/session.jsonl");
        fs::create_dir_all(source_file.parent().unwrap()).unwrap();
        fs::write(&source_file, b"content").unwrap();
        let root = temp.path().join("backup");
        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "claude-code".into(),
                component: ComponentKind::Sessions,
                slot: "projects".into(),
                path: source_file,
                host_version: Some("fixture-v1".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let captured = capture_sources(&config, &root, &AppArgs::default(), Some(&home)).unwrap();
        let id = captured["snapshot_id"].as_str().unwrap();
        let manifest = verify_local(&root, id).unwrap();

        // Attempt traversal mapping
        let bad_mapping = format!("projects={}/../../escaped", home.display());
        let err = build_install_plan(&manifest, "claude-code", Some(&home), &[bad_mapping], &root)
            .unwrap_err();
        assert!(err.to_string().contains("traversal") || err.to_string().contains("escapes"));
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

        let restic = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tmp/native-tools/restic_0.19.1_windows_amd64.exe");
        if !restic.is_file() {
            return;
        }

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
    fn recover_interrupted_install_on_apply_and_succeeds() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let source_file = home.join("source/session.jsonl");
        fs::create_dir_all(source_file.parent().unwrap()).unwrap();
        fs::write(&source_file, b"valid new session content\n").unwrap();

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
        let snap_id = captured["snapshot_id"].as_str().unwrap();
        let manifest = verify_local(&root, snap_id).unwrap();

        // 1. Normal plan generation
        let plan = build_install_plan(&manifest, "grok", Some(&home), &[], &root).unwrap();
        assert_eq!(plan.actions.len(), 1);

        // 2. Manually construct interrupted install state with an older interrupted snapshot
        let interrupted_snap_id = "interrupted-snapshot-123";
        let rollback_dir = root.join("rollback").join(interrupted_snap_id);
        fs::create_dir_all(&rollback_dir).unwrap();

        // Action 1: "新建目标" (target did not exist initially) - half written
        let half_written_target = home.join(".grok/sessions/half_written.jsonl");
        fs::create_dir_all(half_written_target.parent().unwrap()).unwrap();
        fs::write(&half_written_target, b"half written uncompleted data").unwrap();

        // Action 2: "已存在目标 + 备份文件" (target existed initially, but was modified/corrupted)
        let existed_target = home.join(".grok/sessions/preexisting.jsonl");
        fs::write(&existed_target, b"corrupted during interrupted install").unwrap();
        let backup_file = rollback_dir.join("0001.backup");
        fs::write(&backup_file, b"safe original content").unwrap();

        let interrupted_journal = InstallJournal {
            snapshot_id: interrupted_snap_id.to_string(),
            app: "grok".into(),
            phase: "applying".into(),
            target_fingerprint_before: "fake-fingerprint".into(),
            target_fingerprint_after: None,
            completed: vec![],
            mutations: vec![
                JournalActionRecord {
                    index: 0,
                    action_kind: InstallActionKind::Copy,
                    target: half_written_target.clone(),
                    target_existed: false,
                    backup_file: None,
                    cursor_mutations: None,
                },
                JournalActionRecord {
                    index: 1,
                    action_kind: InstallActionKind::Copy,
                    target: existed_target.clone(),
                    target_existed: true,
                    backup_file: Some(backup_file),
                    cursor_mutations: None,
                },
            ],
        };
        atomic_json(&rollback_dir.join("journal.json"), &interrupted_journal).unwrap();

        // 3. Call apply
        let res = apply_install_plan(&root, plan, true).unwrap();

        // Assertions:
        // - 半截文件被删
        assert!(
            !half_written_target.exists(),
            "half written file must be deleted"
        );
        // - 已存在目标恢复为备份内容
        assert_eq!(
            fs::read(&existed_target).unwrap(),
            b"safe original content",
            "preexisting file must be restored from backup"
        );
        // - 旧 journal 的 phase 变为 "rolled-back-after-interruption"
        let updated_journal: InstallJournal =
            serde_json::from_str(&fs::read_to_string(rollback_dir.join("journal.json")).unwrap())
                .unwrap();
        assert_eq!(updated_journal.phase, "rolled-back-after-interruption");
        // - 返回 JSON 含该 snapshot_id
        let recovered = res["recovered_interrupted"].as_array().unwrap();
        assert!(
            recovered
                .iter()
                .any(|v| v.as_str() == Some(interrupted_snap_id)),
            "result must contain interrupted snapshot_id"
        );
        // - 且安装最终成功完成
        assert_eq!(res["file_verification"], "passed");
        let installed_file = home.join(".grok/sessions/session.jsonl");
        assert_eq!(
            fs::read(&installed_file).unwrap(),
            b"valid new session content\n"
        );
    }

    #[test]
    fn dry_run_and_plan_generation_do_not_trigger_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let source_file = home.join("source/session.jsonl");
        fs::create_dir_all(source_file.parent().unwrap()).unwrap();
        fs::write(&source_file, b"session content\n").unwrap();

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
        let snap_id = captured["snapshot_id"].as_str().unwrap();

        // Interrupted state
        let interrupted_snap_id = "interrupted-snapshot-dryrun";
        let rollback_dir = root.join("rollback").join(interrupted_snap_id);
        fs::create_dir_all(&rollback_dir).unwrap();

        let half_written_target = home.join(".grok/sessions/half_written.jsonl");
        fs::create_dir_all(half_written_target.parent().unwrap()).unwrap();
        fs::write(&half_written_target, b"half written uncompleted data").unwrap();

        let existed_target = home.join(".grok/sessions/preexisting.jsonl");
        fs::write(&existed_target, b"corrupted during interrupted install").unwrap();
        let backup_file = rollback_dir.join("0001.backup");
        fs::write(&backup_file, b"safe original content").unwrap();

        let interrupted_journal = InstallJournal {
            snapshot_id: interrupted_snap_id.to_string(),
            app: "grok".into(),
            phase: "applying".into(),
            target_fingerprint_before: "fake-fingerprint".into(),
            target_fingerprint_after: None,
            completed: vec![],
            mutations: vec![
                JournalActionRecord {
                    index: 0,
                    action_kind: InstallActionKind::Copy,
                    target: half_written_target.clone(),
                    target_existed: false,
                    backup_file: None,
                    cursor_mutations: None,
                },
                JournalActionRecord {
                    index: 1,
                    action_kind: InstallActionKind::Copy,
                    target: existed_target.clone(),
                    target_existed: true,
                    backup_file: Some(backup_file),
                    cursor_mutations: None,
                },
            ],
        };
        atomic_json(&rollback_dir.join("journal.json"), &interrupted_journal).unwrap();

        // 1. Plan generation via restore_snapshot(into: "grok")
        let plan_args = RestoreArgs {
            snapshot_id: Some(snap_id.to_string()),
            target: None,
            into: Some("grok".to_string()),
            dry_run: false,
            apply_plan: None,
            map: vec![],
            force: false,
        };
        let plan_res = restore_snapshot(&root, plan_args, Some(&home)).unwrap();
        assert_eq!(plan_res["mode"], "plan-ready");
        let plan_path_str = plan_res["apply_plan"].as_str().unwrap();

        // Verify NO recovery occurred during plan generation:
        assert!(
            half_written_target.exists(),
            "half written target must remain untouched"
        );
        assert_eq!(
            fs::read(&existed_target).unwrap(),
            b"corrupted during interrupted install",
            "existed target must remain untouched"
        );
        let journal_check: InstallJournal =
            serde_json::from_str(&fs::read_to_string(rollback_dir.join("journal.json")).unwrap())
                .unwrap();
        assert_eq!(
            journal_check.phase, "applying",
            "journal must remain in applying phase"
        );

        // 2. Dry run with into + dry_run: true
        let dry_run_into_args = RestoreArgs {
            snapshot_id: Some(snap_id.to_string()),
            target: None,
            into: Some("grok".to_string()),
            dry_run: true,
            apply_plan: None,
            map: vec![],
            force: false,
        };
        let dry_run_res = restore_snapshot(&root, dry_run_into_args, Some(&home)).unwrap();
        assert_eq!(dry_run_res["mode"], "install-dry-run");

        // 3. Dry run with apply_plan + dry_run: true
        let dry_run_apply_args = RestoreArgs {
            snapshot_id: None,
            target: None,
            into: None,
            dry_run: true,
            apply_plan: Some(PathBuf::from(plan_path_str)),
            map: vec![],
            force: false,
        };
        let dry_run_apply_res = restore_snapshot(&root, dry_run_apply_args, Some(&home)).unwrap();
        assert_eq!(dry_run_apply_res["mode"], "apply-dry-run");

        // Final verification: still NO recovery!
        assert!(
            half_written_target.exists(),
            "half written target must still exist after dry-runs"
        );
        assert_eq!(
            fs::read(&existed_target).unwrap(),
            b"corrupted during interrupted install",
            "existed target must remain unchanged after dry-runs"
        );
        let final_journal: InstallJournal =
            serde_json::from_str(&fs::read_to_string(rollback_dir.join("journal.json")).unwrap())
                .unwrap();
        assert_eq!(
            final_journal.phase, "applying",
            "journal must still be applying"
        );
    }

    #[test]
    fn test_registry_verified_allows_install_and_revoke_refuses() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let source_dir = home.join("source/codex");
        fs::create_dir_all(source_dir.join("sessions")).unwrap();
        fs::write(
            source_dir.join("sessions/session.jsonl"),
            b"{\"type\":\"session\"}\n",
        )
        .unwrap();
        let root = temp.path().join("backup");
        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "codex".into(),
                component: ComponentKind::Sessions,
                slot: "sessions".into(),
                path: source_dir.join("sessions"),
                host_version: Some("9.9.9".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let captured = capture_sources(&config, &root, &AppArgs::default(), Some(&home)).unwrap();
        let id = captured["snapshot_id"].as_str().unwrap();
        let manifest = verify_local(&root, id).unwrap();

        let target_dir = home.join("target/codex/sessions");
        let mappings = vec![format!("sessions={}", target_dir.display())];

        // 1. Unregistered version 9.9.9 fails closed on build_install_plan
        let err =
            build_install_plan(&manifest, "codex", Some(&home), &mappings, &root).unwrap_err();
        assert!(
            err.to_string()
                .contains("native installation has not been verified")
        );

        // 2. Unregistered version fails on apply even with force=true
        let forged_plan = InstallPlan {
            kind: InstallPlanKind::Install,
            version: VERSION,
            snapshot_id: manifest.snapshot_id.clone(),
            app: "codex".into(),
            host_version: "9.9.9".into(),
            drill: false,
            target_fingerprint: install_fingerprint(&[InstallAction {
                kind: InstallActionKind::Copy,
                source: "codex/sessions/session.jsonl".into(),
                target: target_dir.join("session.jsonl"),
            }])
            .unwrap(),
            actions: vec![InstallAction {
                kind: InstallActionKind::Copy,
                source: "codex/sessions/session.jsonl".into(),
                target: target_dir.join("session.jsonl"),
            }],
            path_map: [(
                "codex/sessions/session.jsonl".into(),
                target_dir.join("session.jsonl"),
            )]
            .into_iter()
            .collect(),
            rollback_dir: root.join("rollback").join(&manifest.snapshot_id),
        };
        let apply_unverified_err = apply_install_plan(&root, forged_plan, true).unwrap_err();
        assert!(
            apply_unverified_err
                .to_string()
                .contains("native installation has not been verified")
        );

        // 3. Register "9.9.9" as verified in verified-hosts.json
        let reg = VerifiedHostsRegistry {
            hosts: vec![VerifiedHostRecord {
                app: "codex".into(),
                version: "9.9.9".into(),
                status: HostRecordStatus::Verified,
                recorded_at: "2026-09-23T00:00:00Z".into(),
                evidence_path: Some("D:/evidence/uat.json".into()),
                evidence_sha256: Some("abcd1234".into()),
                reason: None,
            }],
        };
        hosts::save_verified_hosts_registry(&root, &reg).unwrap();

        // 4. Now build_install_plan succeeds and produces a normal plan
        let plan = build_install_plan(&manifest, "codex", Some(&home), &mappings, &root).unwrap();
        assert!(!plan.drill);
        assert_eq!(plan.host_version, "9.9.9");

        // 5. Apply plan succeeds
        let res = apply_install_plan(&root, plan.clone(), true).unwrap();
        assert_eq!(res["file_verification"], "passed");
        assert!(target_dir.join("session.jsonl").is_file());

        // 6. Revoke version 9.9.9
        hosts::handle_hosts_revoke(
            &root,
            HostsRevokeArgs {
                app: "codex".into(),
                version: "9.9.9".into(),
                reason: Some("compromised client version".into()),
            },
        )
        .unwrap();

        // 7. After revocation, building new plan is refused
        let revoke_err =
            build_install_plan(&manifest, "codex", Some(&home), &mappings, &root).unwrap_err();
        assert!(
            revoke_err
                .to_string()
                .contains("native installation has not been verified")
        );

        // 8. Applying previously built plan is also refused even with force=true
        let apply_revoke_err = apply_install_plan(&root, plan, true).unwrap_err();
        assert!(
            apply_revoke_err
                .to_string()
                .contains("native installation has not been verified")
        );
    }

    #[test]
    fn test_hosts_record_validation_and_failures() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("backup");
        fs::create_dir_all(&root).unwrap();
        let evidence_dir = temp.path().join("evidence");
        fs::create_dir_all(&evidence_dir).unwrap();
        let evidence_file = evidence_dir.join("evidence.json");

        let make_evidence = |records: &[serde_json::Value]| {
            fs::write(&evidence_file, serde_json::to_vec_pretty(records).unwrap()).unwrap();
        };

        let valid_steps = vec![
            serde_json::json!({"step": "preflight", "exit_code": 0, "result": "passed"}),
            serde_json::json!({"step": "capture", "exit_code": 0, "result": "passed", "host_version": "5.4.3"}),
            serde_json::json!({"step": "replicate", "exit_code": 0, "result": "passed", "host_version": "5.4.3"}),
            serde_json::json!({"step": "verify", "exit_code": 0, "result": "passed", "host_version": "5.4.3"}),
            serde_json::json!({"step": "remove-source", "exit_code": 0, "result": "passed", "host_version": "5.4.3"}),
            serde_json::json!({"step": "restore-install", "exit_code": 0, "result": "passed", "host_version": "5.4.3"}),
            serde_json::json!({"step": "open", "exit_code": 0, "result": "passed", "host_version": "5.4.3"}),
            serde_json::json!({"step": "restart", "exit_code": 0, "result": "passed", "host_version": "5.4.3"}),
            serde_json::json!({"step": "continue", "exit_code": 0, "result": "passed", "host_version": "5.4.3"}),
            serde_json::json!({"step": "summary", "exit_code": 0, "result": "uat-passed", "host_version": "5.4.3", "app": "codex"}),
        ];

        // 1. Success case: valid evidence records verified entry
        make_evidence(&valid_steps);
        let res = hosts::handle_hosts_record(
            &root,
            HostsRecordArgs {
                app: "codex".into(),
                evidence: evidence_file.clone(),
            },
        )
        .unwrap();
        assert_eq!(res["app"], "codex");
        assert_eq!(res["version"], "5.4.3");
        assert_eq!(res["status"], "verified");

        let reg = hosts::load_verified_hosts_registry(&root).unwrap();
        assert_eq!(reg.hosts.len(), 1);
        assert_eq!(reg.hosts[0].status, HostRecordStatus::Verified);
        let initial_reg_count = reg.hosts.len();

        // 2. Failure: missing step (e.g. missing 'verify')
        let mut missing_step = valid_steps.clone();
        missing_step.retain(|s| s["step"] != "verify");
        make_evidence(&missing_step);
        let err = hosts::handle_hosts_record(
            &root,
            HostsRecordArgs {
                app: "codex".into(),
                evidence: evidence_file.clone(),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("missing required step: 'verify'"));
        assert_eq!(
            hosts::load_verified_hosts_registry(&root)
                .unwrap()
                .hosts
                .len(),
            initial_reg_count
        );

        // 3. Failure: step failed
        let mut step_failed = valid_steps.clone();
        for item in &mut step_failed {
            if item["step"] == "restore-install" {
                item["result"] = serde_json::json!("failed");
            }
        }
        make_evidence(&step_failed);
        let err = hosts::handle_hosts_record(
            &root,
            HostsRecordArgs {
                app: "codex".into(),
                evidence: evidence_file.clone(),
            },
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("step 'restore-install' result is Some(\"failed\")")
        );
        assert_eq!(
            hosts::load_verified_hosts_registry(&root)
                .unwrap()
                .hosts
                .len(),
            initial_reg_count
        );

        // 4. Failure: exit code non-zero
        let mut exit_code_fail = valid_steps.clone();
        for item in &mut exit_code_fail {
            if item["step"] == "capture" {
                item["exit_code"] = serde_json::json!(1);
            }
        }
        make_evidence(&exit_code_fail);
        let err = hosts::handle_hosts_record(
            &root,
            HostsRecordArgs {
                app: "codex".into(),
                evidence: evidence_file.clone(),
            },
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("step 'capture' exit_code is Some(1)")
        );
        assert_eq!(
            hosts::load_verified_hosts_registry(&root)
                .unwrap()
                .hosts
                .len(),
            initial_reg_count
        );

        // 5. Failure: inconsistent host_version
        let mut inconsistent_ver = valid_steps.clone();
        for item in &mut inconsistent_ver {
            if item["step"] == "replicate" {
                item["host_version"] = serde_json::json!("9.9.9");
            }
        }
        make_evidence(&inconsistent_ver);
        let err = hosts::handle_hosts_record(
            &root,
            HostsRecordArgs {
                app: "codex".into(),
                evidence: evidence_file.clone(),
            },
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("inconsistent host_versions across steps")
        );
        assert_eq!(
            hosts::load_verified_hosts_registry(&root)
                .unwrap()
                .hosts
                .len(),
            initial_reg_count
        );

        // 6. Failure: summary is not uat-passed
        let mut summary_incomplete = valid_steps.clone();
        for item in &mut summary_incomplete {
            if item["step"] == "summary" {
                item["result"] = serde_json::json!("uat-incomplete");
            }
        }
        make_evidence(&summary_incomplete);
        let err = hosts::handle_hosts_record(
            &root,
            HostsRecordArgs {
                app: "codex".into(),
                evidence: evidence_file.clone(),
            },
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("step 'summary' result is Some(\"uat-incomplete\")")
        );
        assert_eq!(
            hosts::load_verified_hosts_registry(&root)
                .unwrap()
                .hosts
                .len(),
            initial_reg_count
        );

        // 7. Failure: host_version is unknown
        let mut ver_unknown = valid_steps.clone();
        for item in &mut ver_unknown {
            item["host_version"] = serde_json::json!("unknown");
        }
        make_evidence(&ver_unknown);
        let err = hosts::handle_hosts_record(
            &root,
            HostsRecordArgs {
                app: "codex".into(),
                evidence: evidence_file.clone(),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("host_version is 'unknown'"));
        assert_eq!(
            hosts::load_verified_hosts_registry(&root)
                .unwrap()
                .hosts
                .len(),
            initial_reg_count
        );

        // 8. Multiple issues listed in a single error message
        let mut multi_issues = valid_steps.clone();
        multi_issues.retain(|s| s["step"] != "verify");
        for item in &mut multi_issues {
            if item["step"] == "capture" {
                item["exit_code"] = serde_json::json!(2);
            }
            if item["step"] == "summary" {
                item["result"] = serde_json::json!("failed");
            }
        }
        make_evidence(&multi_issues);
        let multi_err = hosts::handle_hosts_record(
            &root,
            HostsRecordArgs {
                app: "codex".into(),
                evidence: evidence_file.clone(),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(multi_err.contains("missing required step: 'verify'"));
        assert!(multi_err.contains("step 'capture' exit_code is Some(2)"));
        assert!(multi_err.contains("step 'summary' result is Some(\"failed\")"));

        // 9. Failure: summary missing required 'app' field
        let mut missing_app = valid_steps.clone();
        for item in &mut missing_app {
            if item["step"] == "summary" {
                item.as_object_mut().unwrap().remove("app");
            }
        }
        make_evidence(&missing_app);
        let err = hosts::handle_hosts_record(
            &root,
            HostsRecordArgs {
                app: "codex".into(),
                evidence: evidence_file.clone(),
            },
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("step 'summary' missing required 'app' field")
        );
        assert_eq!(
            hosts::load_verified_hosts_registry(&root)
                .unwrap()
                .hosts
                .len(),
            initial_reg_count
        );

        // 10. Failure: summary app mismatch (e.g. cursor evidence recorded for codex)
        let mut wrong_app = valid_steps.clone();
        for item in &mut wrong_app {
            if item["step"] == "summary" {
                item["app"] = serde_json::json!("cursor");
            }
        }
        make_evidence(&wrong_app);
        let err = hosts::handle_hosts_record(
            &root,
            HostsRecordArgs {
                app: "codex".into(),
                evidence: evidence_file.clone(),
            },
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("step 'summary' app is 'cursor' (expected 'codex')")
        );
        assert_eq!(
            hosts::load_verified_hosts_registry(&root)
                .unwrap()
                .hosts
                .len(),
            initial_reg_count
        );

        // 11. Success: summary app case-insensitive match (e.g. CODEX vs codex)
        let mut case_app = valid_steps.clone();
        for item in &mut case_app {
            if item["step"] == "summary" {
                item["app"] = serde_json::json!("CODEX");
            }
        }
        make_evidence(&case_app);
        let res = hosts::handle_hosts_record(
            &root,
            HostsRecordArgs {
                app: "codex".into(),
                evidence: evidence_file,
            },
        )
        .unwrap();
        assert_eq!(res["app"], "codex");
    }

    #[test]
    fn test_drill_mode_execution_and_safeguards() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let drill_root = temp.path().join("drill_sandbox");
        fs::create_dir_all(&drill_root).unwrap();
        let drill_marker = drill_root.join(".chronicle-drill");
        fs::write(&drill_marker, b"marker").unwrap();

        let source_dir = home.join("source/codex");
        fs::create_dir_all(source_dir.join("sessions")).unwrap();
        fs::write(
            source_dir.join("sessions/session.jsonl"),
            b"{\"type\":\"session\"}\n",
        )
        .unwrap();

        let root = temp.path().join("backup");
        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "codex".into(),
                component: ComponentKind::Sessions,
                slot: "sessions".into(),
                path: source_dir.join("sessions"),
                host_version: Some("unverified-7.7.7".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let captured = capture_sources(&config, &root, &AppArgs::default(), Some(&home)).unwrap();
        let id = captured["snapshot_id"].as_str().unwrap();
        let manifest = verify_local(&root, id).unwrap();

        // 1. Drill targets inside drill_root containing .chronicle-drill: succeeds with drill: true
        let isolated_target = drill_root.join("codex_home/sessions");
        let mappings = vec![format!("sessions={}", isolated_target.display())];

        let plan = build_install_plan(&manifest, "codex", Some(&home), &mappings, &root).unwrap();
        assert!(plan.drill);
        assert_eq!(plan.host_version, "unverified-7.7.7");

        let res = apply_install_plan_with_home(&root, plan.clone(), true, Some(&home)).unwrap();
        assert_eq!(res["file_verification"], "passed");
        assert!(isolated_target.join("session.jsonl").is_file());

        // 2. Missing marker: if target directory has NO .chronicle-drill in any ancestor, build fails
        let no_marker_dir = temp.path().join("no_marker_dir/sessions");
        let no_marker_mappings = vec![format!("sessions={}", no_marker_dir.display())];
        let err = build_install_plan(&manifest, "codex", Some(&home), &no_marker_mappings, &root)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("native installation has not been verified")
        );
        assert!(err.to_string().contains(".chronicle-drill"));

        // 3. Marker deleted before apply: build succeeds when marker exists, but apply fails if marker is removed
        let plan_to_break =
            build_install_plan(&manifest, "codex", Some(&home), &mappings, &root).unwrap();
        assert!(plan_to_break.drill);
        fs::remove_file(&drill_marker).unwrap(); // delete marker
        let apply_fail =
            apply_install_plan_with_home(&root, plan_to_break, true, Some(&home)).unwrap_err();
        assert!(
            apply_fail
                .to_string()
                .contains("native installation has not been verified")
        );
        assert!(apply_fail.to_string().contains(".chronicle-drill"));

        // Restore marker for remaining tests
        fs::write(&drill_marker, b"marker").unwrap();

        // 4. Protection of real home: target inside default client data root in user home is refused even if marker exists
        let home_target = home.join(".codex/sessions");
        fs::create_dir_all(&home_target).unwrap();
        // Even if home has a .chronicle-drill marker
        fs::write(home.join(".chronicle-drill"), b"marker").unwrap();
        let home_mappings = vec![format!("sessions={}", home_target.display())];
        let home_err =
            build_install_plan(&manifest, "codex", Some(&home), &home_mappings, &root).unwrap_err();
        assert!(
            home_err
                .to_string()
                .contains("native installation has not been verified")
        );
        assert!(
            home_err
                .to_string()
                .contains("falls within default client data root")
        );

        // 5. Revoked version is permitted in drill mode
        hosts::handle_hosts_revoke(
            &root,
            HostsRevokeArgs {
                app: "codex".into(),
                version: "unverified-7.7.7".into(),
                reason: Some("testing re-drill".into()),
            },
        )
        .unwrap();

        let drill_target2 = drill_root.join("drill_run2/sessions");
        let mappings2 = vec![format!("sessions={}", drill_target2.display())];
        let plan2 = build_install_plan(&manifest, "codex", Some(&home), &mappings2, &root).unwrap();
        assert!(plan2.drill);
        let res2 = apply_install_plan_with_home(&root, plan2, true, Some(&home)).unwrap();
        assert_eq!(res2["file_verification"], "passed");
        assert!(drill_target2.join("session.jsonl").is_file());
    }

    #[test]
    fn test_hosts_check_and_status_integration() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("backup");
        fs::create_dir_all(&root).unwrap();

        // 1. Initial status has hosts_check as null when hosts-check.json is absent
        let st_initial = status(&root).unwrap();
        assert!(st_initial["hosts_check"].is_null());

        // 2. Set up registry with 1 verified and 1 revoked
        let reg = VerifiedHostsRegistry {
            hosts: vec![
                VerifiedHostRecord {
                    app: "codex".into(),
                    version: "1.0.0".into(),
                    status: HostRecordStatus::Verified,
                    recorded_at: "2026-09-23T00:00:00Z".into(),
                    evidence_path: None,
                    evidence_sha256: None,
                    reason: None,
                },
                VerifiedHostRecord {
                    app: "claude-code".into(),
                    version: "2.0.0".into(),
                    status: HostRecordStatus::Revoked,
                    recorded_at: "2026-09-23T01:00:00Z".into(),
                    evidence_path: None,
                    evidence_sha256: None,
                    reason: Some("revoked".into()),
                },
            ],
        };
        hosts::save_verified_hosts_registry(&root, &reg).unwrap();

        // 3. Config with 3 apps: codex (verified), grok (unverified), claude-code (revoked)
        let config = NativeConfig {
            sources: vec![
                SourceConfig {
                    app: "codex".into(),
                    component: ComponentKind::Sessions,
                    slot: "sessions".into(),
                    path: PathBuf::from("sessions"),
                    host_version: Some("1.0.0".into()),
                    ..Default::default()
                },
                SourceConfig {
                    app: "grok".into(),
                    component: ComponentKind::Sessions,
                    slot: "sessions".into(),
                    path: PathBuf::from("sessions"),
                    host_version: Some("3.0.0".into()),
                    ..Default::default()
                },
                SourceConfig {
                    app: "claude-code".into(),
                    component: ComponentKind::Sessions,
                    slot: "projects".into(),
                    path: PathBuf::from("projects"),
                    host_version: Some("2.0.0".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        // 4. Run handle_hosts_check
        let check_res = hosts::handle_hosts_check(&config, &root, None).unwrap();
        let hosts_arr = check_res["hosts"].as_array().unwrap();
        assert_eq!(hosts_arr.len(), 3);

        let find_app = |app_name: &str| hosts_arr.iter().find(|h| h["app"] == app_name).unwrap();

        let codex_item = find_app("codex");
        assert_eq!(codex_item["version"], "1.0.0");
        assert_eq!(codex_item["status"], "verified");

        let grok_item = find_app("grok");
        assert_eq!(grok_item["version"], "3.0.0");
        assert_eq!(grok_item["status"], "unverified");

        let claude_item = find_app("claude-code");
        assert_eq!(claude_item["version"], "2.0.0");
        assert_eq!(claude_item["status"], "revoked");

        // 5. hosts-check.json is written
        let check_file = root.join("hosts-check.json");
        assert!(check_file.is_file());

        // 6. status() reads hosts_check without re-probing
        let st = status(&root).unwrap();
        assert!(!st["hosts_check"].is_null());
        assert_eq!(st["hosts_check"]["hosts"].as_array().unwrap().len(), 3);
    }
}
