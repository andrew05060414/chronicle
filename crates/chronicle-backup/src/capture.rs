use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::{ComponentKind, Manifest, SourceConfig};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    #[default]
    Unknown,
    SqliteSessionProjection,
    SqliteDb,
    FileTree,
    SingleFile,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "kebab-case")]
pub enum VersionProvenance {
    #[default]
    Unknown,
    Probed,
    Verified,
    Configured,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureStatus {
    #[default]
    Unknown,
    Captured,
    Unchanged,
    Failed,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "kebab-case")]
pub enum FileRole {
    #[default]
    Unknown,
    Database,
    SessionProjection,
    SessionRollout,
    SessionIndex,
    RegularFile,
}

pub fn default_component() -> ComponentKind {
    ComponentKind::Sessions
}

pub fn detect_source_kind(path: &Path, app: &str) -> SourceKind {
    if app == "cursor" && path.extension().is_some_and(|e| e == "vscdb") {
        SourceKind::SqliteSessionProjection
    } else if path
        .extension()
        .is_some_and(|e| matches!(e.to_str(), Some("db" | "sqlite" | "vscdb")))
    {
        SourceKind::SqliteDb
    } else if path.is_dir() {
        SourceKind::FileTree
    } else if path.is_file() {
        SourceKind::SingleFile
    } else {
        SourceKind::Unknown
    }
}

pub fn detect_file_role(relative_path: &str, app: &str, slot: &str) -> FileRole {
    let path = Path::new(relative_path);
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if app == "cursor" && file_name == "state.vscdb" {
        FileRole::SessionProjection
    } else if app == "codex" && slot == "index" {
        FileRole::SessionIndex
    } else if app == "codex" && slot == "state" {
        FileRole::Database
    } else if file_name.ends_with(".jsonl") {
        FileRole::SessionRollout
    } else if path
        .extension()
        .is_some_and(|e| matches!(e.to_str(), Some("db" | "sqlite" | "vscdb")))
    {
        FileRole::Database
    } else {
        FileRole::RegularFile
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotExplanation {
    pub snapshot_id: String,
    pub device: String,
    pub app_instance: String,
    pub captured_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub sources: Vec<ExplainedSource>,
    pub files: Vec<ExplainedFile>,
    pub incremental: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExplainedSource {
    pub app: String,
    pub component: ComponentKind,
    pub slot: String,
    pub original_path: PathBuf,
    pub source_kind: SourceKind,
    pub host_version: Option<String>,
    pub version_provenance: VersionProvenance,
    pub host_instance: Option<String>,
    pub dependencies: Vec<String>,
    pub capture_status: CaptureStatus,
    pub relative_paths: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExplainedFile {
    pub relative_path: String,
    pub app: String,
    pub slot: String,
    pub component: ComponentKind,
    pub source_kind: SourceKind,
    pub host_version: Option<String>,
    pub version_provenance: VersionProvenance,
    pub host_instance: Option<String>,
    pub role: FileRole,
    pub bytes: u64,
    pub sha256: String,
    pub consistency: String,
    pub dependencies: Vec<String>,
}

/// Explains a snapshot entirely from its snapshot directory without any live-source probing.
pub fn explain_snapshot(snapshot_dir: &Path) -> Result<SnapshotExplanation> {
    let manifest_path = snapshot_dir.join("manifest.json");
    ensure!(
        manifest_path.is_file(),
        "manifest.json missing from snapshot directory: {}",
        snapshot_dir.display()
    );
    let manifest: Manifest = serde_json::from_str(&fs::read_to_string(&manifest_path)?)
        .with_context(|| format!("failed to parse manifest at {}", manifest_path.display()))?;

    explain_manifest(&manifest, snapshot_dir)
}

/// Explains an in-memory or loaded Manifest against a snapshot directory.
pub fn explain_manifest(manifest: &Manifest, snapshot_dir: &Path) -> Result<SnapshotExplanation> {
    let mut explained_sources = Vec::new();
    for source in &manifest.sources {
        explained_sources.push(ExplainedSource {
            app: source.app.clone(),
            component: source.component,
            slot: source.slot.clone(),
            original_path: source.path.clone(),
            source_kind: source.source_kind,
            host_version: source.host_version.clone(),
            version_provenance: source.version_provenance,
            host_instance: source.host_instance.clone(),
            dependencies: source.dependencies.clone(),
            capture_status: source.capture_status,
            relative_paths: source.relative_paths.clone(),
        });
    }

    let mut explained_files = Vec::new();
    for file in &manifest.files {
        let (matched_source, rel_sub) = find_matching_source(&manifest.sources, &file.relative);
        let app = matched_source
            .map(|s| s.app.clone())
            .unwrap_or_else(|| derive_app_from_relative(&file.relative));
        let slot = matched_source
            .map(|s| s.slot.clone())
            .unwrap_or_else(|| derive_slot_from_relative(&file.relative));
        let component = matched_source
            .map(|s| s.component)
            .unwrap_or(ComponentKind::Sessions);
        let source_kind = matched_source
            .map(|s| s.source_kind)
            .unwrap_or(SourceKind::Unknown);
        let host_version = matched_source.and_then(|s| s.host_version.clone());
        let version_provenance = matched_source
            .map(|s| s.version_provenance)
            .unwrap_or(VersionProvenance::Unknown);
        let host_instance = matched_source.and_then(|s| s.host_instance.clone());
        let role = if file.role != FileRole::Unknown {
            file.role
        } else {
            detect_file_role(&rel_sub, &app, &slot)
        };
        let dependencies = if !file.dependencies.is_empty() {
            file.dependencies.clone()
        } else if let Some(src) = matched_source {
            src.dependencies.clone()
        } else {
            Vec::new()
        };

        // Check file exists in snapshot directory and verify hash/size
        let on_disk = snapshot_dir.join(&file.relative);
        ensure!(
            on_disk.is_file(),
            "snapshot file missing from disk: {}",
            on_disk.display()
        );
        let (bytes, hash) = hash_file_path(&on_disk)?;
        ensure!(
            bytes == file.bytes && hash == file.sha256,
            "hash/size mismatch for snapshot file {}: expected ({}, {}), got ({}, {})",
            file.relative,
            file.bytes,
            file.sha256,
            bytes,
            hash
        );

        explained_files.push(ExplainedFile {
            relative_path: file.relative.clone(),
            app,
            slot,
            component,
            source_kind,
            host_version,
            version_provenance,
            host_instance,
            role,
            bytes: file.bytes,
            sha256: file.sha256.clone(),
            consistency: file.consistency.clone(),
            dependencies,
        });
    }

    Ok(SnapshotExplanation {
        snapshot_id: manifest.snapshot_id.clone(),
        device: manifest.device.clone(),
        app_instance: manifest.app_instance.clone(),
        captured_at: manifest.captured_at,
        finished_at: manifest.finished_at,
        sources: explained_sources,
        files: explained_files,
        incremental: manifest.incremental,
    })
}

fn find_matching_source<'a>(
    sources: &'a [SourceConfig],
    relative: &str,
) -> (Option<&'a SourceConfig>, String) {
    for source in sources {
        let prefix = format!("{}/{}/", source.app, source.slot);
        if let Some(sub) = relative.strip_prefix(&prefix) {
            return (Some(source), sub.to_string());
        }
    }
    (None, relative.to_string())
}

fn derive_app_from_relative(relative: &str) -> String {
    relative.split('/').next().unwrap_or("unknown").to_string()
}

fn derive_slot_from_relative(relative: &str) -> String {
    let mut parts = relative.split('/');
    parts.next();
    parts.next().unwrap_or("unknown").to_string()
}

pub fn hash_file_path(path: &Path) -> Result<(u64, String)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    let mut bytes = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes += read as u64;
    }
    let hash = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((bytes, hash))
}

/// Tracks file identity and modification timestamp to detect mid-copy changes.
#[derive(Debug, Clone)]
pub struct SourceFileStamp {
    pub len: u64,
    pub modified: std::time::SystemTime,
}

impl SourceFileStamp {
    pub fn of(path: &Path) -> Result<Self> {
        let meta = fs::metadata(path)
            .with_context(|| format!("failed to read metadata for {}", path.display()))?;
        Ok(Self {
            len: meta.len(),
            modified: meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
        })
    }

    pub fn assert_unmodified(&self, path: &Path) -> Result<()> {
        let current = Self::of(path)?;
        ensure!(
            self.len == current.len,
            "file size changed mid-copy for {}: was {} bytes, now {} bytes",
            path.display(),
            self.len,
            current.len
        );
        ensure!(
            self.modified == current.modified,
            "file modified timestamp changed mid-copy for {}",
            path.display()
        );
        Ok(())
    }
}

/// Persistent watch queue state tracking pending sources, reasons, and event timestamps.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct WatchQueue {
    pub pending: BTreeMap<String, WatchQueueEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatchQueueEntry {
    pub app: String,
    pub slot: String,
    pub source_path: PathBuf,
    pub reasons: Vec<String>,
    pub first_event_at: DateTime<Utc>,
    pub last_event_at: DateTime<Utc>,
    pub attempts: u32,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// Persistent replication state tracking targets, attempts, and errors.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ReplicationState {
    pub failures: BTreeMap<String, ReplicationFailureEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplicationFailureEntry {
    pub target: String,
    pub snapshot_id: String,
    pub attempts: u32,
    pub last_error: String,
    pub last_error_at: DateTime<Utc>,
    pub last_success_at: Option<DateTime<Utc>>,
}

pub fn state_dir(data_root: &Path) -> PathBuf {
    data_root.join("state")
}

pub fn load_watch_queue(data_root: &Path) -> Result<WatchQueue> {
    let path = state_dir(data_root).join("watch_queue.json");
    if !path.is_file() {
        return Ok(WatchQueue::default());
    }
    let content = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&content).unwrap_or_default())
}

pub fn save_watch_queue(data_root: &Path, queue: &WatchQueue) -> Result<()> {
    let dir = state_dir(data_root);
    fs::create_dir_all(&dir)?;
    let path = dir.join("watch_queue.json");
    let temp = dir.join("watch_queue.json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(queue)?)?;
    fs::rename(temp, path)?;
    Ok(())
}

pub fn load_replication_state(data_root: &Path) -> Result<ReplicationState> {
    let path = state_dir(data_root).join("replication_failures.json");
    if !path.is_file() {
        return Ok(ReplicationState::default());
    }
    let content = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&content).unwrap_or_default())
}

pub fn save_replication_state(data_root: &Path, state: &ReplicationState) -> Result<()> {
    let dir = state_dir(data_root);
    fs::create_dir_all(&dir)?;
    let path = dir.join("replication_failures.json");
    let temp = dir.join("replication_failures.json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(state)?)?;
    fs::rename(temp, path)?;
    Ok(())
}

/// Pure timing decision for watch debouncing.
/// - 5s quiet after the last event
/// - 60s hard upper bound from the first event (continuous writes must merge)
/// - 300s (5min) fallback reconciliation even without events
pub fn capture_due_at(
    now: Instant,
    last_capture: Instant,
    first_event: Option<Instant>,
    last_event: Instant,
) -> bool {
    let since_capture = now.saturating_duration_since(last_capture);
    let since_first = first_event.map(|first| now.saturating_duration_since(first));
    let since_event = now.saturating_duration_since(last_event);
    crate::capture_due(since_capture, since_first, since_event)
}

/// Probes a command with a hard timeout, killing the child process if it exceeds the deadline.
pub fn command_version_timeout(command: &str, timeout: Duration) -> Option<String> {
    let mut child = Command::new(command)
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_end(&mut stdout);
                }
                if let Some(mut err) = child.stderr.take() {
                    let _ = err.read_to_end(&mut stderr);
                }
                let text = format!(
                    "{} {}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr)
                );
                let line = text.lines().find(|line| !line.trim().is_empty())?.trim();
                if line.len() > 160 {
                    return None;
                }
                return Some(line.to_string());
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}
