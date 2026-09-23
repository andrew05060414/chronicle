//! Remote sync functionality over SSH.
//!
//! Provides fetching and bidirectional merging of hstry databases across machines.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Connection;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::config::RemoteConfig;
use crate::db::{Database, SearchOptions};
use crate::error::{Error, Result};
use crate::models::{Conversation, ConversationWithMessages, Message, Source};
use crate::recall::SearchReport;

/// Default remote database path (XDG standard).
pub const DEFAULT_REMOTE_DB_PATH: &str = "~/.local/share/hstry/hstry.db";

#[derive(Debug, Deserialize)]
struct JsonResponse<T> {
    ok: bool,
    result: Option<T>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct RemoteSearchInput {
    query: String,
    limit: Option<i64>,
    offset: Option<i64>,
    source: Option<String>,
    workspace: Option<String>,
    mode: Option<String>,
    after: Option<String>,
    before: Option<String>,
    role: Option<Vec<String>>,
    model: Option<String>,
    harness_filter: Option<String>,
    tag: Option<String>,
}

#[derive(Debug, Serialize)]
struct RemoteShowInput {
    id: String,
}

/// Result of a fetch operation.
#[derive(Debug, Clone, Serialize)]
pub struct FetchResult {
    pub remote_name: String,
    pub local_cache_path: PathBuf,
    pub bytes_transferred: u64,
    pub fetched_at: DateTime<Utc>,
}

/// Result of a sync/merge operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResult {
    pub remote_name: String,
    pub conversations_added: usize,
    pub conversations_updated: usize,
    pub messages_added: usize,
    pub sources_added: usize,
    pub sources_updated: usize,
    pub direction: SyncDirection,
}

/// Sync direction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SyncDirection {
    /// Pull from remote to local.
    Pull,
    /// Push from local to remote.
    Push,
    /// Bidirectional merge.
    Bidirectional,
}

impl std::fmt::Display for SyncDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncDirection::Pull => write!(f, "pull"),
            SyncDirection::Push => write!(f, "push"),
            SyncDirection::Bidirectional => write!(f, "bidirectional"),
        }
    }
}

/// SSH transport for remote operations.
pub struct SshTransport {
    host: String,
    port: Option<u16>,
    identity_file: Option<String>,
}

impl SshTransport {
    /// Create a new SSH transport from remote config.
    pub fn from_config(config: &RemoteConfig) -> Self {
        Self {
            host: config.host.clone(),
            port: config.port,
            identity_file: config.identity_file.clone(),
        }
    }

    /// Build SSH command with common options.
    fn ssh_command(&self) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=accept-new")
            .arg("-o")
            .arg("ConnectTimeout=10");

        if let Some(port) = self.port {
            cmd.arg("-p").arg(port.to_string());
        }

        if let Some(ref identity) = self.identity_file {
            let expanded = shellexpand::full(identity)
                .map_or_else(|_| identity.clone(), std::borrow::Cow::into_owned);
            cmd.arg("-i").arg(expanded);
        }

        cmd
    }

    /// Build SCP command with common options.
    fn scp_command(&self) -> Command {
        let mut cmd = Command::new("scp");
        cmd.arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=accept-new")
            .arg("-o")
            .arg("ConnectTimeout=10")
            .arg("-C"); // Enable compression

        if let Some(port) = self.port {
            cmd.arg("-P").arg(port.to_string());
        }

        if let Some(ref identity) = self.identity_file {
            let expanded = shellexpand::full(identity)
                .map_or_else(|_| identity.clone(), std::borrow::Cow::into_owned);
            cmd.arg("-i").arg(expanded);
        }

        cmd
    }

    /// Test connection to the remote host.
    pub fn test_connection(&self) -> Result<()> {
        let mut cmd = self.ssh_command();
        cmd.arg(&self.host).arg("echo").arg("ok");

        let output = cmd
            .output()
            .map_err(|e| Error::Remote(format!("Failed to execute ssh: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Remote(format!(
                "SSH connection failed: {}",
                stderr.trim()
            )));
        }

        Ok(())
    }

    /// Fetch a file from the remote host to a local path.
    pub fn fetch_file(&self, remote_path: &str, local_path: &Path) -> Result<u64> {
        // Ensure parent directory exists
        if let Some(parent) = local_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Expand remote path (shell expansion happens on remote)
        let remote_spec = scp_remote_target(&self.host, remote_path);

        let mut cmd = self.scp_command();
        cmd.arg(&remote_spec).arg(local_path);

        let output = cmd
            .output()
            .map_err(|e| Error::Remote(format!("Failed to execute scp: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Remote(format!(
                "SCP fetch failed: {}",
                stderr.trim()
            )));
        }

        // Return file size
        let metadata = std::fs::metadata(local_path)?;
        Ok(metadata.len())
    }

    /// Push a file from local to remote.
    pub fn push_file(&self, local_path: &Path, remote_path: &str) -> Result<u64> {
        let metadata = std::fs::metadata(local_path)?;
        let size = metadata.len();

        let remote_spec = scp_remote_target(&self.host, remote_path);

        let mut cmd = self.scp_command();
        cmd.arg(local_path).arg(&remote_spec);

        let output = cmd
            .output()
            .map_err(|e| Error::Remote(format!("Failed to execute scp: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Remote(format!("SCP push failed: {}", stderr.trim())));
        }

        Ok(size)
    }

    /// Execute a command on the remote host and return stdout.
    pub fn exec(&self, command: &str) -> Result<String> {
        let mut cmd = self.ssh_command();
        cmd.arg(&self.host).arg(command);

        let output = cmd
            .output()
            .map_err(|e| Error::Remote(format!("Failed to execute ssh: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Remote(format!(
                "Remote command failed: {}",
                stderr.trim()
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Check if a file exists on the remote.
    pub fn file_exists(&self, remote_path: &str) -> Result<bool> {
        let cmd = file_exists_command(remote_path);
        let output = self.exec(&cmd)?;
        Ok(output.trim() == "yes")
    }

    /// Get the expanded path on the remote (resolves ~ and env vars).
    pub fn expand_remote_path(&self, path: &str) -> Result<String> {
        let cmd = expand_remote_path_command(path);
        let output = self.exec(&cmd)?;
        Ok(output.trim().to_string())
    }
}

/// Quote a path for safe use in remote shell commands.
fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Build one shell word that expands leading `~`, `$VAR`, and `${VAR}` while
/// keeping every other character literal. Variable names are restricted to
/// portable shell identifiers, so operators and command substitutions can
/// never become executable syntax.
fn remote_path_expression(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut expression = String::new();
    let mut index = 0;

    if path == "~" || path.starts_with("~/") {
        expression.push_str("\"${HOME}\"");
        index = 1;
    }

    let mut literal_start = index;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }

        let (name_start, name_end, token_end) = if bytes.get(index + 1) == Some(&b'{') {
            let name_start = index + 2;
            let Some(relative_end) = bytes[name_start..].iter().position(|byte| *byte == b'}')
            else {
                index += 1;
                continue;
            };
            let name_end = name_start + relative_end;
            (name_start, name_end, name_end + 1)
        } else {
            let name_start = index + 1;
            let mut name_end = name_start;
            while bytes
                .get(name_end)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                name_end += 1;
            }
            (name_start, name_end, name_end)
        };

        let name = &bytes[name_start..name_end];
        let valid_name = name
            .first()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
            && name
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
        if !valid_name {
            index += 1;
            continue;
        }

        if literal_start < index {
            expression.push_str(&shell_quote(&path[literal_start..index]));
        }
        expression.push('"');
        expression.push_str(&path[index..token_end]);
        expression.push('"');
        index = token_end;
        literal_start = index;
    }

    if literal_start < path.len() {
        expression.push_str(&shell_quote(&path[literal_start..]));
    }
    if expression.is_empty() {
        expression.push_str("''");
    }

    expression
}

fn expand_remote_path_command(path: &str) -> String {
    format!("printf '%s\\n' {}", remote_path_expression(path))
}

fn file_exists_command(path: &str) -> String {
    format!(
        "test -f {} && printf 'yes\\n' || printf 'no\\n'",
        shell_quote(path)
    )
}

/// Build an SCP remote target (`user@host:/path with spaces/file`).
///
/// The path is passed as a single `scp` argv element, so spaces are safe without
/// shell quoting. Do not wrap the path in quotes — Windows OpenSSH scp treats
/// `host:'/path'` as a literal path and fails to find the file.
fn scp_remote_target(host: &str, remote_path: &str) -> String {
    format!("{host}:{remote_path}")
}

/// Get the cache directory for remote databases.
pub fn remote_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hstry")
        .join("remotes")
}

/// Get the cached database path for a remote.
pub fn cached_db_path(remote_name: &str) -> PathBuf {
    remote_cache_dir().join(format!("{remote_name}.db"))
}

/// Fetch a remote database to local cache.
pub fn fetch_remote(config: &RemoteConfig) -> Result<FetchResult> {
    let transport = SshTransport::from_config(config);

    // Test connection first
    transport.test_connection()?;

    // Determine remote database path
    let remote_db_path = config
        .database_path
        .as_deref()
        .unwrap_or(DEFAULT_REMOTE_DB_PATH);

    // Expand the path on the remote
    let expanded_path = transport.expand_remote_path(remote_db_path)?;

    // Check if remote database exists
    if !transport.file_exists(&expanded_path)? {
        return Err(Error::Remote(format!(
            "Remote database not found at: {expanded_path}",
        )));
    }

    // Fetch to local cache
    let cache_path = cached_db_path(&config.name);
    let bytes = transport.fetch_file(&expanded_path, &cache_path)?;

    Ok(FetchResult {
        remote_name: config.name.clone(),
        local_cache_path: cache_path,
        bytes_transferred: bytes,
        fetched_at: Utc::now(),
    })
}

/// Merge satellite source metadata into an existing hub source.
///
/// Returns `None` when the incoming record is stale (older `last_sync_at`) so an
/// old device cannot roll hub timestamps backward. When accepted, refreshes
/// `last_sync_at` (max of existing/incoming), `config` (including sync cursor),
/// `path`, and `adapter` from the incoming record.
fn merge_source_metadata(existing: &Source, incoming: &Source) -> Option<Source> {
    let incoming_ts = incoming.last_sync_at?;

    if existing
        .last_sync_at
        .is_some_and(|existing_ts| incoming_ts < existing_ts)
    {
        return None;
    }

    let merged_last_sync = match existing.last_sync_at {
        Some(existing_ts) if existing_ts > incoming_ts => Some(existing_ts),
        _ => Some(incoming_ts),
    };

    Some(Source {
        id: existing.id.clone(),
        adapter: incoming.adapter.clone(),
        path: incoming.path.clone(),
        last_sync_at: merged_last_sync,
        config: incoming.config.clone(),
    })
}

fn source_metadata_changed(before: &Source, after: &Source) -> bool {
    before.last_sync_at != after.last_sync_at
        || before.config != after.config
        || before.path != after.path
        || before.adapter != after.adapter
}

/// Whether `source_id` is already namespaced to `namespace`.
///
/// Hub rows are stored as `"<device>:<source>"`, so a satellite's own rows
/// come back as `arknights:cursor-abc` when it pulls the hub.
fn belongs_to_namespace(source_id: &str, namespace: &str) -> bool {
    match source_id.strip_prefix(namespace) {
        Some("") => true,
        Some(rest) => rest.starts_with(':'),
        None => false,
    }
}

/// Merge conversations from a source database into a target database.
/// Uses updated_at for conflict resolution (newer wins).
pub async fn merge_databases(
    target: &Database,
    source_path: &Path,
    remote_name: &str,
) -> Result<SyncResult> {
    merge_databases_excluding(target, source_path, remote_name, None).await
}

/// Merge like [`merge_databases`], skipping rows that already belong to
/// `own_namespace`.
///
/// A satellite pushes its archive to the hub under its own device namespace,
/// so the hub holds `arknights:cursor-abc`. Merging the hub back in prefixes
/// every id a second time (`nas-lan:arknights:cursor-abc`) and mints fresh
/// conversation and message ids, so no later deduplication can match: the
/// satellite ends up storing a full second copy of itself, and every search
/// spends half its result window on that echo.
///
/// Pull passes its own device namespace here, which leaves those rows on the
/// hub they came from; rows from other devices still merge normally. Push
/// passes `None` -- namespacing local data into the hub is what it is for.
pub async fn merge_databases_excluding(
    target: &Database,
    source_path: &Path,
    remote_name: &str,
    own_namespace: Option<&str>,
) -> Result<SyncResult> {
    // Open the source database
    let source = Database::open(source_path).await?;

    let mut conversations_added = 0usize;
    let mut conversations_updated = 0usize;
    let mut messages_added = 0usize;
    let mut sources_added = 0usize;
    let mut sources_updated = 0usize;

    // Merge sources (prefixed with remote name to avoid conflicts)
    let remote_sources = source.list_sources().await?;
    let mut sources_skipped = 0usize;
    for mut remote_source in remote_sources {
        if let Some(own) = own_namespace
            && belongs_to_namespace(&remote_source.id, own)
        {
            sources_skipped += 1;
            continue;
        }
        // Prefix source ID with remote name to namespace it
        let namespaced_id = format!("{}:{}", remote_name, remote_source.id);
        remote_source.id = namespaced_id;

        // Check if source already exists
        let existing = target.get_source(&remote_source.id).await?;
        match existing {
            None => {
                target.upsert_source(&remote_source).await?;
                sources_added += 1;
            }
            Some(existing_source) => {
                match merge_source_metadata(&existing_source, &remote_source) {
                    Some(merged) if source_metadata_changed(&existing_source, &merged) => {
                        target.upsert_source(&merged).await?;
                        sources_updated += 1;
                    }
                    _ => {}
                }
            }
        }
    }

    // Get all conversations from source
    let source_conversations = source
        .list_conversations(crate::db::ListConversationsOptions::default())
        .await?;

    // Collect conversations and messages to insert, then write in a single transaction
    let mut batch_convs: Vec<Conversation> = Vec::new();
    let mut batch_msgs: Vec<Message> = Vec::new();
    let mut affected_ids: Vec<Uuid> = Vec::new();

    let mut conversations_skipped = 0usize;
    for conv in source_conversations {
        if let Some(own) = own_namespace
            && belongs_to_namespace(&conv.source_id, own)
        {
            conversations_skipped += 1;
            continue;
        }
        // Namespace the source_id
        let namespaced_source_id = format!("{}:{}", remote_name, conv.source_id);

        // Check if conversation already exists (by external_id within namespaced source, or by direct id match)
        let existing_id = if let Some(ref external_id) = conv.external_id {
            target
                .get_conversation_id(&namespaced_source_id, external_id)
                .await?
        } else if let Ok(Some(existing_conv)) = target.get_conversation(conv.id).await {
            if existing_conv.source_id == namespaced_source_id {
                Some(conv.id)
            } else {
                None
            }
        } else {
            None
        };

        let (should_insert, conv_id) = if let Some(existing_uuid) = existing_id {
            // Conversation exists, check if we should update
            if let Some(existing_conv) = target.get_conversation(existing_uuid).await? {
                // Compare updated_at timestamps (newer wins)
                let should_update = match (conv.updated_at, existing_conv.updated_at) {
                    (Some(new_ts), Some(old_ts)) => {
                        new_ts > old_ts
                            || (new_ts == old_ts && conv.version > existing_conv.version)
                    }
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => {
                        conv.created_at > existing_conv.created_at
                            || (conv.created_at == existing_conv.created_at
                                && conv.version > existing_conv.version)
                    }
                };
                if should_update {
                    conversations_updated += 1;
                    (true, existing_uuid)
                } else {
                    (false, existing_uuid)
                }
            } else {
                (true, existing_uuid)
            }
        } else {
            // New conversation
            conversations_added += 1;
            (true, conv.id)
        };

        if should_insert {
            let source_messages = source.get_messages(conv.id).await?;
            let mut metadata = conv.metadata.clone();
            if !metadata.is_object() {
                metadata = serde_json::json!({});
            }
            metadata["hstry_sync"] = serde_json::json!({
                "machine": remote_name,
                "captured_at": Utc::now(),
                "message_count": source_messages.len(),
            });
            let merged_conv = Conversation {
                id: conv_id,
                source_id: namespaced_source_id.clone(),
                external_id: conv.external_id,
                readable_id: conv.readable_id,
                platform_id: conv.platform_id,
                title: conv.title,
                created_at: conv.created_at,
                updated_at: conv.updated_at,
                model: conv.model,
                provider: conv.provider,
                workspace: conv.workspace,
                tokens_in: conv.tokens_in,
                tokens_out: conv.tokens_out,
                cost_usd: conv.cost_usd,
                metadata,
                harness: conv.harness,
                version: 0,
                message_count: 0,
                parent_conversation_id: conv.parent_conversation_id,
                parent_message_idx: conv.parent_message_idx,
                fork_type: conv.fork_type,
            };

            affected_ids.push(conv_id);
            batch_convs.push(merged_conv);

            // Collect messages from the same snapshot described by hstry_sync.
            for msg in source_messages {
                let merged_msg = Message {
                    id: Uuid::new_v4(),
                    conversation_id: conv_id,
                    idx: msg.idx,
                    role: msg.role,
                    content: msg.content,
                    parts_json: msg.parts_json,
                    created_at: msg.created_at,
                    model: msg.model,
                    tokens: msg.tokens,
                    cost_usd: msg.cost_usd,
                    metadata: msg.metadata,
                    sender: msg.sender,
                    provider: msg.provider,
                    harness: msg.harness,
                    client_id: msg.client_id,
                };
                batch_msgs.push(merged_msg);
                messages_added += 1;
            }
        }
    }

    // Write everything in a single transaction
    if !batch_convs.is_empty() {
        let mut tx = target.begin().await?;
        for conv in &batch_convs {
            target.upsert_conversation_in_tx(&mut tx, conv).await?;
        }
        for msg in &batch_msgs {
            target.insert_message_in_tx(&mut tx, msg).await?;
        }
        tx.commit().await?;

        // Rebuild caches outside the transaction
        target.rebuild_conversation_summaries(&affected_ids).await?;
    }

    source.close().await;

    if sources_skipped > 0 || conversations_skipped > 0 {
        tracing::info!(
            remote = remote_name,
            namespace = own_namespace.unwrap_or_default(),
            sources_skipped,
            conversations_skipped,
            "skipped rows already namespaced to this device"
        );
    }

    Ok(SyncResult {
        remote_name: remote_name.to_string(),
        conversations_added,
        conversations_updated,
        messages_added,
        sources_added,
        sources_updated,
        direction: SyncDirection::Pull,
    })
}

/// Result of exporting a satellite delta sqlite.
#[derive(Debug, Clone, Serialize)]
pub struct DeltaExport {
    pub conversations: usize,
    pub messages: usize,
    pub sources: usize,
    pub empty: bool,
    /// Inclusive capture boundary used for the next watermark (change cursor).
    pub export_cut: i64,
}

pub fn push_watermark_key(remote_name: &str) -> String {
    format!("push_watermark:{remote_name}")
}

/// Copy conversations/sources changed since `confirmed_cursor` into `dest_path`.
pub async fn export_delta(
    source: &Database,
    dest_path: &Path,
    confirmed_cursor: Option<i64>,
) -> Result<DeltaExport> {
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if dest_path.exists() {
        std::fs::remove_file(dest_path)?;
    }

    let snapshot_dir = tempfile::tempdir()?;
    let snapshot_path = snapshot_dir.path().join("source-snapshot.db");
    source.backup_to(&snapshot_path).await?;
    let source_snapshot = Database::open(&snapshot_path).await?;
    let cut = source_snapshot.get_max_local_change().await?.unwrap_or(0);

    let convs = if let Some(since) = confirmed_cursor {
        let changed_ids = source_snapshot
            .get_changed_conversation_ids(since, cut)
            .await?;
        source_snapshot
            .get_conversations_by_ids(&changed_ids)
            .await?
    } else {
        source_snapshot
            .list_conversations(crate::db::ListConversationsOptions::default())
            .await?
    };

    let conv_source_ids: HashSet<String> = convs.iter().map(|c| c.source_id.clone()).collect();
    let all_sources = source_snapshot.list_sources().await?;
    let sources_to_copy: Vec<_> = all_sources
        .into_iter()
        .filter(|s| {
            if confirmed_cursor.is_none() {
                return true;
            }
            conv_source_ids.contains(&s.id)
        })
        .collect();

    if convs.is_empty() && sources_to_copy.is_empty() {
        source_snapshot.close().await;
        let _ = std::fs::remove_file(snapshot_path);
        return Ok(DeltaExport {
            conversations: 0,
            messages: 0,
            sources: 0,
            empty: true,
            export_cut: cut,
        });
    }

    let dest = Database::open(dest_path).await?;
    for source_row in &sources_to_copy {
        dest.upsert_source(source_row).await?;
    }

    let mut messages = 0usize;
    for conv in &convs {
        dest.upsert_conversation(conv).await?;
        for msg in source_snapshot.get_messages(conv.id).await? {
            dest.insert_message(&msg).await?;
            messages += 1;
        }
    }

    source_snapshot.close().await;
    dest.close().await;
    let _ = std::fs::remove_file(snapshot_path);
    Ok(DeltaExport {
        conversations: convs.len(),
        messages,
        sources: sources_to_copy.len(),
        empty: false,
        export_cut: cut,
    })
}

/// Merge a satellite delta into the live hub database under `namespace`.
pub async fn ingest_into_hub(
    hub: &Database,
    delta_path: &Path,
    namespace: &str,
    lock_path: &Path,
) -> Result<SyncResult> {
    let _lock = crate::checkpoint::acquire_ingest_lock(lock_path)?;
    let mut result = merge_databases(hub, delta_path, namespace).await?;
    result.direction = SyncDirection::Push;
    Ok(result)
}

fn remote_parent_dir(remote_db_path: &str) -> String {
    match remote_db_path.rfind('/') {
        Some(idx) if idx > 0 => remote_db_path[..idx].to_string(),
        _ => ".".to_string(),
    }
}

fn remote_inbox_dir(remote_db_path: &str) -> String {
    format!("{}/inbox", remote_parent_dir(remote_db_path))
}

/// Full sync operation: fetch remote DB and merge into local.
pub async fn sync_from_remote(
    local_db: &Database,
    config: &RemoteConfig,
    device_namespace: &str,
) -> Result<(FetchResult, SyncResult)> {
    // Fetch the remote database
    let fetch_result = fetch_remote(config)?;

    // Merge into local, leaving this device's own rows on the hub.
    let namespace = crate::config::sanitize_device_namespace(device_namespace);
    let sync_result = merge_databases_excluding(
        local_db,
        &fetch_result.local_cache_path,
        &config.name,
        Some(&namespace),
    )
    .await?;

    Ok((fetch_result, sync_result))
}

/// Push local staging into the hub.
///
/// Default path exports a delta sqlite, SCPs it to the hub inbox, and runs
/// `hstry hub ingest` on the remote so the live file is never overwritten.
/// `--full` keeps the legacy fetch/merge/SCP-replace path for disaster recovery.
pub async fn sync_to_remote(
    local_db: &Database,
    local_db_path: &Path,
    config: &RemoteConfig,
    device_namespace: &str,
    full: bool,
) -> Result<SyncResult> {
    if full {
        return sync_to_remote_full(local_db, local_db_path, config, device_namespace).await;
    }

    let transport = SshTransport::from_config(config);
    transport.test_connection()?;

    let namespace = crate::config::sanitize_device_namespace(device_namespace);
    let watermark: Option<i64> = match local_db
        .get_search_state(&push_watermark_key(&config.name))
        .await?
    {
        Some(raw) => raw.trim().parse::<i64>().ok(),
        None => None,
    };
    let temp_dir = tempfile::tempdir()?;
    let delta_path = temp_dir.path().join("delta.db");
    let export = export_delta(local_db, &delta_path, watermark).await?;
    if export.empty {
        // No-change skip: record contact, but do not advance the push watermark.
        local_db
            .record_push_contact(&config.name, Utc::now())
            .await?;
        return Ok(SyncResult {
            remote_name: config.name.clone(),
            conversations_added: 0,
            conversations_updated: 0,
            messages_added: 0,
            sources_added: 0,
            sources_updated: 0,
            direction: SyncDirection::Push,
        });
    }

    let remote_db_path = config
        .database_path
        .as_deref()
        .unwrap_or(DEFAULT_REMOTE_DB_PATH);
    let expanded_path = transport.expand_remote_path(remote_db_path)?;
    let inbox = remote_inbox_dir(&expanded_path);
    transport.exec(&format!("mkdir -p {}", shell_quote(&inbox)))?;

    let remote_delta = format!("{inbox}/{namespace}-{}.db", export.export_cut);
    transport.push_file(&delta_path, &remote_delta)?;

    let ingest_cmd = format!(
        "hstry --json hub ingest --file {} --namespace {} --delete",
        shell_quote(&remote_delta),
        shell_quote(&namespace)
    );
    let stdout = match transport.exec(&ingest_cmd) {
        Ok(s) => s,
        Err(err) => {
            let msg = err.to_string();
            let hub_too_old = msg.contains("unrecognized subcommand")
                || msg.contains("unexpected argument 'hub'")
                || msg.contains("command not found");
            if hub_too_old {
                tracing::warn!(
                    error = %msg,
                    "hub ingest not available on remote; falling back to whole-file push"
                );
                let _ = transport.exec(&format!("rm -f {}", shell_quote(&remote_delta)));
                return sync_to_remote_full(local_db, local_db_path, config, device_namespace)
                    .await;
            }
            return Err(err);
        }
    };
    let response: JsonResponse<SyncResult> = serde_json::from_str(stdout.trim()).map_err(|e| {
        Error::Remote(format!(
            "Failed parsing hub ingest response: {e}; stdout={}",
            stdout.trim()
        ))
    })?;
    if !response.ok {
        return Err(Error::Remote(
            response
                .error
                .unwrap_or_else(|| "hub ingest failed".to_string()),
        ));
    }
    let mut sync_result = response
        .result
        .ok_or_else(|| Error::Remote("hub ingest returned ok without a result".to_string()))?;
    sync_result.remote_name = config.name.clone();
    sync_result.direction = SyncDirection::Push;

    // Advance ONLY after hub commit succeeded. Use the export cut cursor.
    local_db
        .record_push_success(&config.name, export.export_cut, Utc::now())
        .await?;

    Ok(sync_result)
}

async fn validate_hstry_database(path: &Path) -> Result<()> {
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .create_if_missing(false);
    let mut connection = sqlx::SqliteConnection::connect_with(&options)
        .await
        .map_err(|error| {
            Error::Remote(format!("Fetched remote database is not SQLite: {error}"))
        })?;

    let quick_check: String = sqlx::query_scalar("PRAGMA quick_check")
        .fetch_one(&mut connection)
        .await
        .map_err(|error| Error::Remote(format!("Remote database check failed: {error}")))?;
    if quick_check != "ok" {
        return Err(Error::Remote(format!(
            "Fetched remote database failed SQLite quick_check: {quick_check}"
        )));
    }

    let has_conversations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'conversations'",
    )
    .fetch_one(&mut connection)
    .await
    .map_err(|error| Error::Remote(format!("Remote database schema check failed: {error}")))?;
    connection.close().await.map_err(|error| {
        Error::Remote(format!("Could not close remote database check: {error}"))
    })?;

    if has_conversations != 1 {
        return Err(Error::Remote(
            "Fetched SQLite file is not a hstry database".to_string(),
        ));
    }
    Ok(())
}

/// Legacy push: fetch hub, merge locally, SCP the whole file back.
pub async fn sync_to_remote_full(
    local_db: &Database,
    local_db_path: &Path,
    config: &RemoteConfig,
    device_namespace: &str,
) -> Result<SyncResult> {
    let cut_cursor = local_db.get_max_local_change().await?.unwrap_or(0);
    let transport = SshTransport::from_config(config);
    transport.test_connection()?;

    let remote_db_path = config
        .database_path
        .as_deref()
        .unwrap_or(DEFAULT_REMOTE_DB_PATH);
    let expanded_path = transport.expand_remote_path(remote_db_path)?;

    let temp_dir = tempfile::tempdir()?;
    let temp_db_path = temp_dir.path().join("merged.db");

    let remote_exists = transport.file_exists(&expanded_path)?;
    if remote_exists {
        transport.fetch_file(&expanded_path, &temp_db_path)?;
        let fetched_len = std::fs::metadata(&temp_db_path).map_err(Error::Io)?.len();
        if fetched_len < 1024 {
            return Err(Error::Remote(format!(
                "Fetched remote database at {expanded_path} is unexpectedly small ({fetched_len} bytes); aborting push to avoid overwriting the hub"
            )));
        }
        validate_hstry_database(&temp_db_path).await?;
    } else if config.database_path.is_some() {
        tracing::warn!(
            remote = %expanded_path,
            "Remote hub database not found; push will create a new hub from local data only"
        );
    }

    let temp_db = Database::open(&temp_db_path).await?;
    let namespace = crate::config::sanitize_device_namespace(device_namespace);
    let sync_result = merge_databases(&temp_db, local_db_path, &namespace).await?;
    temp_db.close().await;

    let incoming = format!("{expanded_path}.incoming");
    transport.push_file(&temp_db_path, &incoming)?;
    transport.exec(&format!(
        "mv -f {} {}",
        shell_quote(&incoming),
        shell_quote(&expanded_path)
    ))?;

    local_db
        .record_push_success(&config.name, cut_cursor, Utc::now())
        .await?;

    Ok(SyncResult {
        remote_name: config.name.clone(),
        conversations_added: sync_result.conversations_added,
        conversations_updated: sync_result.conversations_updated,
        messages_added: sync_result.messages_added,
        sources_added: sync_result.sources_added,
        sources_updated: sync_result.sources_updated,
        direction: SyncDirection::Push,
    })
}

pub async fn search_remote(
    config: &RemoteConfig,
    query: &str,
    opts: &SearchOptions,
) -> Result<SearchReport> {
    let transport = SshTransport::from_config(config);
    let input = RemoteSearchInput {
        query: query.to_string(),
        limit: opts.limit,
        offset: opts.offset,
        source: opts.source_id.clone(),
        workspace: opts.workspace.clone(),
        mode: Some(opts.mode.label().to_string()),
        after: opts.after.map(|d| d.to_rfc3339()),
        before: opts.before.map(|d| d.to_rfc3339()),
        role: opts
            .role
            .as_ref()
            .map(|r| r.split(',').map(str::to_owned).collect()),
        model: opts.model.clone(),
        harness_filter: opts.harness.clone(),
        tag: opts.tag.clone(),
    };
    let payload = serde_json::to_vec(&input)?;
    let host_name = config.name.clone();
    let host = config.host.clone();

    let hits = tokio::task::spawn_blocking(move || {
        let mut cmd = transport.ssh_command();
        cmd.arg(host)
            .arg("hstry")
            .arg("search")
            .arg("--json")
            .arg("--raw")
            .arg("--include-system")
            .arg("--input")
            .arg("-");

        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Error::Remote(format!("Failed to start ssh: {e}")))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(&payload)
                .map_err(|e| Error::Remote(format!("Failed writing stdin: {e}")))?;
        }

        let output = child
            .wait_with_output()
            .map_err(|e| Error::Remote(format!("SSH failed: {e}")))?;

        if !output.status.success() {
            return Err(Error::Remote(format!(
                "Remote search failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let response: JsonResponse<SearchReport> = serde_json::from_slice(&output.stdout)
            .map_err(|e| Error::Remote(format!("Failed parsing remote response: {e}")))?;

        if !response.ok {
            return Err(Error::Remote(
                response
                    .error
                    .unwrap_or_else(|| "Remote search error".to_string()),
            ));
        }

        Ok(response.result.unwrap_or_default())
    })
    .await
    .map_err(|e| Error::Remote(format!("Remote search join error: {e}")))??;

    let mut report = hits;
    report.scope = format!("remote:{host_name}");
    for hit in &mut report.hits {
        hit.host = Some(host_name.clone());
        hit.provenance.machine = Some(host_name.clone());
    }
    for store in &mut report.stores {
        store.machine = Some(host_name.clone());
    }
    Ok(report)
}

pub async fn search_remotes(
    remotes: &[RemoteConfig],
    query: &str,
    opts: &SearchOptions,
) -> Result<SearchReport> {
    let mut set = JoinSet::new();
    for remote in remotes.iter().filter(|r| r.enabled) {
        let remote = remote.clone();
        let query = query.to_string();
        let opts = opts.clone();
        set.spawn(async move {
            let name = remote.name.clone();
            (name, search_remote(&remote, &query, &opts).await)
        });
    }

    let mut hits = SearchReport {
        scope: "remote".into(),
        ..Default::default()
    };
    let mut successes = 0usize;
    let mut failures = 0usize;
    while let Some(result) = set.join_next().await {
        match result {
            Ok((_name, Ok(remote))) => {
                successes += 1;
                crate::recall::merge_reports(&mut hits, remote);
            }
            Ok((name, Err(err))) => {
                failures += 1;
                hits.warnings.push(format!(
                    "Coverage error: remote '{name}' search failed: {err}"
                ));
                hits.stores.push(crate::recall::Provenance {
                    source: format!("remote:{name}"),
                    machine: Some(name),
                    completeness: "unknown".into(),
                    last_error: Some(err.to_string()),
                    last_contact_at: Some(Utc::now()),
                    ..Default::default()
                });
            }
            Err(err) => {
                failures += 1;
                hits.warnings
                    .push(format!("Coverage error: remote search task failed: {err}"));
            }
        }
    }

    if successes == 0 && failures > 0 {
        return Err(Error::Remote(
            hits.warnings
                .last()
                .cloned()
                .unwrap_or_else(|| "All remote searches failed".into()),
        ));
    }
    crate::recall::fold_exact_duplicates(&mut hits.hits);
    Ok(hits)
}

/// Read on the source. Older/unbounded peers are rejected, never silently hydrated.
pub async fn read_remote(
    config: &RemoteConfig,
    id: &str,
    options: &crate::read::ReadOptions,
) -> Result<crate::read::ReadPage> {
    let mut options = options.clone();
    options.machine = Some(config.name.clone());
    options.validate()?;
    let max_chars = options.max_chars;
    let payload = serde_json::to_vec(&serde_json::json!({"id":id,"options":options}))?;
    let transport = SshTransport::from_config(config);
    let host = config.host.clone();
    tokio::task::spawn_blocking(move || {
        let mut command=transport.ssh_command();
        let mut child=command.arg(host).arg("hstry read --json --input -").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn()?;
        if let Some(mut stdin)=child.stdin.take() {stdin.write_all(&payload)?;}
        let mut output=Vec::new();
        let cap=max_chars*4+1;
        if let Some(stdout)=child.stdout.take() {stdout.take(cap as u64).read_to_end(&mut output)?;}
        if output.len()>=cap {let _=child.kill();let _=child.wait();return Err(Error::Remote("Peer exceeded bounded read protocol; upgrade the peer".into()));}
        let status=child.wait()?;
        if !status.success() {return Err(Error::Remote("Remote bounded read failed; verify ID/options and upgrade the peer if read is unsupported".into()));}
        let response:JsonResponse<crate::read::ReadPage>=serde_json::from_slice(&output).map_err(|_|Error::Remote("Incompatible bounded read response; upgrade the peer".into()))?;
        let page=response.result.filter(|p|response.ok && p.protocol==1 && p.machine==options.machine).ok_or_else(||Error::Remote("Peer rejected bounded read protocol".into()))?;
        if String::from_utf8_lossy(&output).chars().count()>max_chars {return Err(Error::Remote("Peer exceeded serialized read budget".into()));}
        Ok(page)
    }).await.map_err(|e|Error::Remote(format!("Remote read task failed: {e}")))?
}

pub async fn show_remote(
    config: &RemoteConfig,
    conversation_id: &str,
) -> Result<ConversationWithMessages> {
    let transport = SshTransport::from_config(config);
    let input = RemoteShowInput {
        id: conversation_id.to_string(),
    };
    let payload = serde_json::to_vec(&input)?;
    let host = config.host.clone();

    tokio::task::spawn_blocking(move || {
        let mut cmd = transport.ssh_command();
        cmd.arg(host)
            .arg("hstry")
            .arg("show")
            .arg("--full")
            .arg("--json")
            .arg("--input")
            .arg("-");

        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Error::Remote(format!("Failed to start ssh: {e}")))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(&payload)
                .map_err(|e| Error::Remote(format!("Failed writing stdin: {e}")))?;
        }

        let output = child
            .wait_with_output()
            .map_err(|e| Error::Remote(format!("SSH failed: {e}")))?;

        if !output.status.success() {
            return Err(Error::Remote(format!(
                "Remote show failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let response: JsonResponse<ConversationWithMessages> =
            serde_json::from_slice(&output.stdout)
                .map_err(|e| Error::Remote(format!("Failed parsing remote response: {e}")))?;

        if !response.ok {
            return Err(Error::Remote(
                response
                    .error
                    .unwrap_or_else(|| "Remote show error".to_string()),
            ));
        }

        response
            .result
            .ok_or_else(|| Error::Remote("Remote show returned no result".to_string()))
    })
    .await
    .map_err(|e| Error::Remote(format!("Remote show join error: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cached_db_path() {
        let path = cached_db_path("laptop");
        assert!(path.to_string_lossy().contains("laptop.db"));
    }

    #[test]
    fn test_ssh_transport_command_building() {
        let config = RemoteConfig {
            name: "test".to_string(),
            host: "user@example.com".to_string(),
            database_path: None,
            port: Some(2222),
            identity_file: Some("~/.ssh/custom_key".to_string()),
            enabled: true,
        };

        let transport = SshTransport::from_config(&config);
        assert_eq!(transport.host, "user@example.com");
        assert_eq!(transport.port, Some(2222));
    }

    #[test]
    fn shell_quote_escapes_spaces_and_single_quotes() {
        assert_eq!(
            shell_quote("/vol1/1000/Code/hstry backup/hstry.db"),
            "'/vol1/1000/Code/hstry backup/hstry.db'"
        );
        assert_eq!(shell_quote("it's fine"), "'it'\"'\"'s fine'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn scp_remote_target_preserves_spaces_without_shell_quotes() {
        assert_eq!(
            scp_remote_target("admin@nas", "/vol1/1000/Code/hstry backup/hstry.db"),
            "admin@nas:/vol1/1000/Code/hstry backup/hstry.db"
        );
    }

    #[test]
    fn expand_remote_path_command_uses_printf_not_eval() {
        let command = expand_remote_path_command("~/db/$HSTRY_DIR/${HOME}/$(touch injected).db");
        assert!(command.starts_with("printf '%s\\n' "));
        assert!(!command.contains("eval"));
        assert!(command.contains("\"${HOME}\""));
        assert!(command.contains("\"$HSTRY_DIR\""));
        assert!(command.contains(&shell_quote("/$(touch injected).db")));
    }

    #[test]
    fn remote_path_expression_quotes_metacharacters_pipes_globs_and_newlines() {
        let path =
            "~/history/$HSTRY_TEST_DIR/it's; touch injected | rm -rf /; $(touch injected)\n*.db";
        let expression = remote_path_expression(path);
        assert!(!expression.contains("eval"));
        assert!(expression.starts_with("\"${HOME}\""));
        assert!(expression.contains("\"$HSTRY_TEST_DIR\""));
        assert!(expression.contains(&shell_quote(
            "/it's; touch injected | rm -rf /; $(touch injected)\n*.db"
        )));
        assert_eq!(
            expand_remote_path_command("/vol1/1000/Code/hstry backup/*.db"),
            format!(
                "printf '%s\\n' {}",
                shell_quote("/vol1/1000/Code/hstry backup/*.db")
            )
        );
    }

    #[test]
    fn remote_file_check_uses_one_shell_argument() {
        let malicious_path = "/missing; touch injected\nsecond | cat";
        let command = file_exists_command(malicious_path);
        assert_eq!(
            command,
            format!(
                "test -f {} && printf 'yes\\n' || printf 'no\\n'",
                shell_quote(malicious_path)
            )
        );
        assert!(!command.contains("eval"));
    }

    #[cfg(unix)]
    fn run_posix_shell(
        command: &str,
        env: &[(&str, &str)],
        cwd: Option<&Path>,
    ) -> std::process::Output {
        let mut child = Command::new("sh");
        child.arg("-c").arg(command);
        for (key, value) in env {
            child.env(key, value);
        }
        if let Some(dir) = cwd {
            child.current_dir(dir);
        }
        child.output().expect("run POSIX shell command")
    }

    #[cfg(unix)]
    #[test]
    fn remote_path_expansion_treats_shell_metacharacters_as_data() {
        let temp = tempfile::tempdir().expect("temp directory");
        let marker = "injected";
        let path =
            format!("~/history/$HSTRY_TEST_DIR/it's; touch {marker}; $(touch {marker})\n.db");
        let command = expand_remote_path_command(&path);

        let output = run_posix_shell(
            &command,
            &[
                ("HOME", "/remote/home"),
                ("HSTRY_TEST_DIR", "folder with spaces"),
            ],
            Some(temp.path()),
        );

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).expect("UTF-8 output"),
            format!(
                "/remote/home/history/folder with spaces/it's; touch {marker}; $(touch {marker})\n.db\n",
            )
        );
        assert!(!temp.path().join(marker).exists());
    }

    #[cfg(unix)]
    #[test]
    fn remote_file_check_treats_expanded_path_as_one_shell_word() {
        let temp = tempfile::tempdir().expect("temp directory");
        let marker = "injected";
        let malicious_path = format!("/missing; touch {marker}\nsecond");
        let command = file_exists_command(&malicious_path);

        let output = run_posix_shell(&command, &[], Some(temp.path()));

        assert!(output.status.success());
        assert_eq!(output.stdout, b"no\n");
        assert!(!temp.path().join(marker).exists());
    }

    #[cfg(unix)]
    #[test]
    fn remote_path_expansion_keeps_spaces_and_globs_literal() {
        let path = "/vol1/1000/Code/hstry backup/*.db";
        let command = expand_remote_path_command(path);

        let output = run_posix_shell(&command, &[], None);

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).expect("UTF-8 output"),
            "/vol1/1000/Code/hstry backup/*.db\n"
        );
    }

    #[tokio::test]
    async fn push_merge_preserves_existing_hub_sources() {
        use crate::models::{MessageRole, Source};

        let hub_dir = tempfile::tempdir().unwrap();
        let hub_path = hub_dir.path().join("hub.db");
        let hub = Database::open(&hub_path).await.unwrap();

        let win_source = Source {
            id: "arknights:cursor-abc".to_string(),
            adapter: "cursor".to_string(),
            path: Some("/win/cursor".to_string()),
            last_sync_at: None,
            config: serde_json::json!({}),
        };
        hub.upsert_source(&win_source).await.unwrap();

        let conv = Conversation {
            id: Uuid::new_v4(),
            source_id: win_source.id.clone(),
            external_id: Some("ext-1".to_string()),
            readable_id: None,
            platform_id: None,
            title: Some("Windows session".to_string()),
            created_at: Utc::now(),
            updated_at: Some(Utc::now()),
            model: None,
            provider: None,
            workspace: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            metadata: serde_json::json!({}),
            harness: None,
            version: 0,
            message_count: 0,
            parent_conversation_id: None,
            parent_message_idx: None,
            fork_type: None,
        };
        hub.upsert_conversation(&conv).await.unwrap();
        hub.insert_message(&Message {
            id: Uuid::new_v4(),
            conversation_id: conv.id,
            idx: 0,
            role: MessageRole::User,
            content: "hello from windows".to_string(),
            parts_json: serde_json::json!({}),
            created_at: Some(Utc::now()),
            model: None,
            tokens: None,
            cost_usd: None,
            metadata: serde_json::json!({}),
            sender: None,
            provider: None,
            harness: None,
            client_id: None,
        })
        .await
        .unwrap();
        hub.close().await;

        let mac_dir = tempfile::tempdir().unwrap();
        let mac_path = mac_dir.path().join("mac.db");
        let mac = Database::open(&mac_path).await.unwrap();

        let mac_source = Source {
            id: "cursor-xyz".to_string(),
            adapter: "cursor".to_string(),
            path: Some("/mac/cursor".to_string()),
            last_sync_at: None,
            config: serde_json::json!({}),
        };
        mac.upsert_source(&mac_source).await.unwrap();

        let mac_conv = Conversation {
            id: Uuid::new_v4(),
            source_id: mac_source.id.clone(),
            external_id: Some("ext-2".to_string()),
            readable_id: None,
            platform_id: None,
            title: Some("Mac session".to_string()),
            created_at: Utc::now(),
            updated_at: Some(Utc::now()),
            model: None,
            provider: None,
            workspace: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            metadata: serde_json::json!({}),
            harness: None,
            version: 0,
            message_count: 0,
            parent_conversation_id: None,
            parent_message_idx: None,
            fork_type: None,
        };
        mac.upsert_conversation(&mac_conv).await.unwrap();
        mac.insert_message(&Message {
            id: Uuid::new_v4(),
            conversation_id: mac_conv.id,
            idx: 0,
            role: MessageRole::User,
            content: "hello from mac".to_string(),
            parts_json: serde_json::json!({}),
            created_at: Some(Utc::now()),
            model: None,
            tokens: None,
            cost_usd: None,
            metadata: serde_json::json!({}),
            sender: None,
            provider: None,
            harness: None,
            client_id: None,
        })
        .await
        .unwrap();
        mac.close().await;

        let merged_dir = tempfile::tempdir().unwrap();
        let merged_path = merged_dir.path().join("merged.db");
        std::fs::copy(&hub_path, &merged_path).unwrap();
        let merged = Database::open(&merged_path).await.unwrap();

        let result = merge_databases(&merged, &mac_path, "macbook")
            .await
            .unwrap();
        assert_eq!(result.conversations_added, 1);

        let sources = merged.list_sources().await.unwrap();
        let source_ids: Vec<_> = sources.iter().map(|s| s.id.as_str()).collect();
        assert!(source_ids.contains(&"arknights:cursor-abc"));
        assert!(source_ids.contains(&"macbook:cursor-xyz"));

        let convs = merged
            .list_conversations(crate::db::ListConversationsOptions::default())
            .await
            .unwrap();
        assert_eq!(convs.len(), 2);
    }

    #[test]
    fn merge_source_metadata_accepts_newer_incoming() {
        let old_ts = Utc::now() - chrono::Duration::days(2);
        let new_ts = Utc::now();

        let existing = Source {
            id: "arknights:cursor-abc".to_string(),
            adapter: "cursor".to_string(),
            path: Some("/win/cursor".to_string()),
            last_sync_at: Some(old_ts),
            config: serde_json::json!({ "cursor": "old" }),
        };
        let incoming = Source {
            id: "arknights:cursor-abc".to_string(),
            adapter: "cursor".to_string(),
            path: Some("/win/cursor".to_string()),
            last_sync_at: Some(new_ts),
            config: serde_json::json!({ "cursor": "new" }),
        };

        let merged = merge_source_metadata(&existing, &incoming).expect("should merge");
        assert_eq!(merged.last_sync_at, Some(new_ts));
        assert_eq!(merged.config["cursor"], "new");
    }

    #[test]
    fn merge_source_metadata_rejects_stale_incoming() {
        let old_ts = Utc::now() - chrono::Duration::days(2);
        let new_ts = Utc::now();

        let existing = Source {
            id: "arknights:cursor-abc".to_string(),
            adapter: "cursor".to_string(),
            path: Some("/win/cursor".to_string()),
            last_sync_at: Some(new_ts),
            config: serde_json::json!({ "cursor": "current" }),
        };
        let incoming = Source {
            id: "arknights:cursor-abc".to_string(),
            adapter: "cursor".to_string(),
            path: Some("/win/cursor-stale".to_string()),
            last_sync_at: Some(old_ts),
            config: serde_json::json!({ "cursor": "stale" }),
        };

        assert!(merge_source_metadata(&existing, &incoming).is_none());
    }

    #[tokio::test]
    async fn export_delta_skips_conversations_older_than_watermark() {
        use crate::models::{MessageRole, Source};

        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("src.db");
        let src = Database::open(&src_path).await.unwrap();
        let source = Source {
            id: "cursor-abc".to_string(),
            adapter: "cursor".to_string(),
            path: Some("/win/cursor".to_string()),
            last_sync_at: Some(Utc::now()),
            config: serde_json::json!({}),
        };
        src.upsert_source(&source).await.unwrap();

        let old_ts = Utc::now() - chrono::Duration::days(2);
        let old_conv = Conversation {
            id: Uuid::new_v4(),
            source_id: source.id.clone(),
            external_id: Some("old".to_string()),
            readable_id: None,
            platform_id: None,
            title: Some("old".to_string()),
            created_at: old_ts,
            updated_at: Some(old_ts),
            model: None,
            provider: None,
            workspace: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            metadata: serde_json::json!({}),
            harness: None,
            version: 0,
            message_count: 0,
            parent_conversation_id: None,
            parent_message_idx: None,
            fork_type: None,
        };
        src.upsert_conversation(&old_conv).await.unwrap();
        src.insert_message(&Message {
            id: Uuid::new_v4(),
            conversation_id: old_conv.id,
            idx: 0,
            role: MessageRole::User,
            content: "old".to_string(),
            parts_json: serde_json::json!({}),
            created_at: Some(old_ts),
            model: None,
            tokens: None,
            cost_usd: None,
            metadata: serde_json::json!({}),
            sender: None,
            provider: None,
            harness: None,
            client_id: None,
        })
        .await
        .unwrap();

        let old_cursor = src.get_max_local_change().await.unwrap().unwrap();

        let new_ts = Utc::now();
        let new_conv = Conversation {
            id: Uuid::new_v4(),
            source_id: source.id.clone(),
            external_id: Some("new".to_string()),
            readable_id: None,
            platform_id: None,
            title: Some("new".to_string()),
            created_at: new_ts,
            updated_at: Some(new_ts),
            model: None,
            provider: None,
            workspace: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            metadata: serde_json::json!({}),
            harness: None,
            version: 0,
            message_count: 0,
            parent_conversation_id: None,
            parent_message_idx: None,
            fork_type: None,
        };
        src.upsert_conversation(&new_conv).await.unwrap();
        src.insert_message(&Message {
            id: Uuid::new_v4(),
            conversation_id: new_conv.id,
            idx: 0,
            role: MessageRole::User,
            content: "new".to_string(),
            parts_json: serde_json::json!({}),
            created_at: Some(new_ts),
            model: None,
            tokens: None,
            cost_usd: None,
            metadata: serde_json::json!({}),
            sender: None,
            provider: None,
            harness: None,
            client_id: None,
        })
        .await
        .unwrap();

        let delta_path = dir.path().join("delta.db");
        let export = export_delta(&src, &delta_path, Some(old_cursor))
            .await
            .unwrap();
        assert_eq!(export.conversations, 1);
        assert!(export.export_cut > old_cursor);
        src.close().await;

        let delta = Database::open(&delta_path).await.unwrap();
        let convs = delta
            .list_conversations(crate::db::ListConversationsOptions::default())
            .await
            .unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].external_id.as_deref(), Some("new"));
        delta.close().await;
    }

    #[tokio::test]
    async fn export_delta_includes_historical_import_after_confirmed_push() {
        use crate::models::Source;

        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("src.db");
        let src = Database::open(&src_path).await.unwrap();
        let source = Source {
            id: "src-hist".to_string(),
            adapter: "cursor".to_string(),
            path: None,
            last_sync_at: None,
            config: serde_json::json!({}),
        };
        src.upsert_source(&source).await.unwrap();

        // 1. Initial conversation
        let now = Utc::now();
        let c1 = Conversation {
            id: Uuid::new_v4(),
            source_id: source.id.clone(),
            external_id: Some("c1".to_string()),
            readable_id: None,
            platform_id: None,
            title: Some("c1".to_string()),
            created_at: now,
            updated_at: Some(now),
            model: None,
            provider: None,
            workspace: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            metadata: serde_json::json!({}),
            harness: None,
            version: 0,
            message_count: 0,
            parent_conversation_id: None,
            parent_message_idx: None,
            fork_type: None,
        };
        src.upsert_conversation(&c1).await.unwrap();

        let delta1_path = dir.path().join("delta1.db");
        let export1 = export_delta(&src, &delta1_path, None).await.unwrap();
        assert_eq!(export1.conversations, 1);
        src.record_push_success("hub", export1.export_cut, Utc::now())
            .await
            .unwrap();

        // 2. Import a conversation with created_at/updated_at years in the past
        let ancient = Utc::now() - chrono::Duration::days(5 * 365);
        let c_hist = Conversation {
            id: Uuid::new_v4(),
            source_id: source.id.clone(),
            external_id: Some("ancient".to_string()),
            readable_id: None,
            platform_id: None,
            title: Some("ancient".to_string()),
            created_at: ancient,
            updated_at: Some(ancient),
            model: None,
            provider: None,
            workspace: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            metadata: serde_json::json!({}),
            harness: None,
            version: 0,
            message_count: 0,
            parent_conversation_id: None,
            parent_message_idx: None,
            fork_type: None,
        };
        src.upsert_conversation(&c_hist).await.unwrap();

        // 3. Next export delta with confirmed watermark export1.export_cut MUST include c_hist!
        let delta2_path = dir.path().join("delta2.db");
        let export2 = export_delta(&src, &delta2_path, Some(export1.export_cut))
            .await
            .unwrap();
        assert_eq!(export2.conversations, 1);

        let delta2 = Database::open(&delta2_path).await.unwrap();
        let exported_convs = delta2
            .list_conversations(crate::db::ListConversationsOptions::default())
            .await
            .unwrap();
        assert_eq!(exported_convs.len(), 1);
        assert_eq!(exported_convs[0].external_id.as_deref(), Some("ancient"));
        delta2.close().await;
        src.close().await;
    }

    #[tokio::test]
    async fn conversation_written_after_snapshot_cut_stays_pending() {
        use crate::models::Source;

        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("src.db");
        let src = Database::open(&src_path).await.unwrap();
        let source = Source {
            id: "src-pending".to_string(),
            adapter: "cursor".to_string(),
            path: None,
            last_sync_at: None,
            config: serde_json::json!({}),
        };
        src.upsert_source(&source).await.unwrap();

        let now = Utc::now();
        let c1 = Conversation {
            id: Uuid::new_v4(),
            source_id: source.id.clone(),
            external_id: Some("c1".to_string()),
            readable_id: None,
            platform_id: None,
            title: Some("c1".to_string()),
            created_at: now,
            updated_at: Some(now),
            model: None,
            provider: None,
            workspace: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            metadata: serde_json::json!({}),
            harness: None,
            version: 0,
            message_count: 0,
            parent_conversation_id: None,
            parent_message_idx: None,
            fork_type: None,
        };
        src.upsert_conversation(&c1).await.unwrap();

        // Snapshot cut happens during export
        let delta_path = dir.path().join("delta.db");
        let export = export_delta(&src, &delta_path, None).await.unwrap();

        // While upload is in flight, write c2
        let c2 = Conversation {
            id: Uuid::new_v4(),
            source_id: source.id.clone(),
            external_id: Some("c2".to_string()),
            readable_id: None,
            platform_id: None,
            title: Some("c2".to_string()),
            created_at: now,
            updated_at: Some(now),
            model: None,
            provider: None,
            workspace: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            metadata: serde_json::json!({}),
            harness: None,
            version: 0,
            message_count: 0,
            parent_conversation_id: None,
            parent_message_idx: None,
            fork_type: None,
        };
        src.upsert_conversation(&c2).await.unwrap();

        // Upload completes for export.export_cut only
        src.record_push_success("hub", export.export_cut, Utc::now())
            .await
            .unwrap();

        // Source must stay pending!
        let updated_source = src.get_source("src-pending").await.unwrap().unwrap();
        let cfg = &updated_source.config;
        assert_eq!(
            cfg.get("last_confirmed_cursor").and_then(|v| v.as_i64()),
            Some(export.export_cut)
        );
        let local_cursor = cfg.get("local_cursor").and_then(|v| v.as_i64()).unwrap();
        assert!(local_cursor > export.export_cut);
        assert_eq!(cfg.get("pending").and_then(|v| v.as_bool()), Some(true));
        src.close().await;
    }

    #[tokio::test]
    async fn ingest_two_namespaces_without_clobbering() {
        use crate::models::{MessageRole, Source};

        let dir = tempfile::tempdir().unwrap();
        let hub_path = dir.path().join("hub.db");
        let hub = Database::open(&hub_path).await.unwrap();

        let win_dir = tempfile::tempdir().unwrap();
        let win_path = win_dir.path().join("win.db");
        let win = Database::open(&win_path).await.unwrap();
        let win_source = Source {
            id: "cursor-win".to_string(),
            adapter: "cursor".to_string(),
            path: Some("/win".to_string()),
            last_sync_at: Some(Utc::now()),
            config: serde_json::json!({}),
        };
        win.upsert_source(&win_source).await.unwrap();
        let win_conv = Conversation {
            id: Uuid::new_v4(),
            source_id: win_source.id.clone(),
            external_id: Some("win-1".to_string()),
            readable_id: None,
            platform_id: None,
            title: Some("win".to_string()),
            created_at: Utc::now(),
            updated_at: Some(Utc::now()),
            model: None,
            provider: None,
            workspace: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            metadata: serde_json::json!({}),
            harness: None,
            version: 0,
            message_count: 0,
            parent_conversation_id: None,
            parent_message_idx: None,
            fork_type: None,
        };
        win.upsert_conversation(&win_conv).await.unwrap();
        win.insert_message(&Message {
            id: Uuid::new_v4(),
            conversation_id: win_conv.id,
            idx: 0,
            role: MessageRole::User,
            content: "from windows".to_string(),
            parts_json: serde_json::json!({}),
            created_at: Some(Utc::now()),
            model: None,
            tokens: None,
            cost_usd: None,
            metadata: serde_json::json!({}),
            sender: None,
            provider: None,
            harness: None,
            client_id: None,
        })
        .await
        .unwrap();
        win.close().await;

        let lock = dir.path().join("hub.ingest.lock");
        ingest_into_hub(&hub, &win_path, "arknights", &lock)
            .await
            .unwrap();

        let mac_dir = tempfile::tempdir().unwrap();
        let mac_path = mac_dir.path().join("mac.db");
        let mac = Database::open(&mac_path).await.unwrap();
        let mac_source = Source {
            id: "cursor-mac".to_string(),
            adapter: "cursor".to_string(),
            path: Some("/mac".to_string()),
            last_sync_at: Some(Utc::now()),
            config: serde_json::json!({}),
        };
        mac.upsert_source(&mac_source).await.unwrap();
        let mac_conv = Conversation {
            id: Uuid::new_v4(),
            source_id: mac_source.id.clone(),
            external_id: Some("mac-1".to_string()),
            readable_id: None,
            platform_id: None,
            title: Some("mac".to_string()),
            created_at: Utc::now(),
            updated_at: Some(Utc::now()),
            model: None,
            provider: None,
            workspace: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            metadata: serde_json::json!({}),
            harness: None,
            version: 0,
            message_count: 0,
            parent_conversation_id: None,
            parent_message_idx: None,
            fork_type: None,
        };
        mac.upsert_conversation(&mac_conv).await.unwrap();
        mac.insert_message(&Message {
            id: Uuid::new_v4(),
            conversation_id: mac_conv.id,
            idx: 0,
            role: MessageRole::User,
            content: "from mac".to_string(),
            parts_json: serde_json::json!({}),
            created_at: Some(Utc::now()),
            model: None,
            tokens: None,
            cost_usd: None,
            metadata: serde_json::json!({}),
            sender: None,
            provider: None,
            harness: None,
            client_id: None,
        })
        .await
        .unwrap();
        mac.close().await;

        ingest_into_hub(&hub, &mac_path, "macbook", &lock)
            .await
            .unwrap();

        let sources = hub.list_sources().await.unwrap();
        let ids: Vec<_> = sources.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"arknights:cursor-win"));
        assert!(ids.contains(&"macbook:cursor-mac"));
        let convs = hub
            .list_conversations(crate::db::ListConversationsOptions::default())
            .await
            .unwrap();
        assert_eq!(convs.len(), 2);
        hub.close().await;
    }

    #[tokio::test]
    async fn second_push_updates_existing_source_metadata_without_adding_source() {
        use crate::models::Source;

        let old_ts = Utc::now() - chrono::Duration::days(2);
        let new_ts = Utc::now();

        let hub_dir = tempfile::tempdir().unwrap();
        let hub_path = hub_dir.path().join("hub.db");
        let hub = Database::open(&hub_path).await.unwrap();

        let hub_source = Source {
            id: "arknights:cursor-abc".to_string(),
            adapter: "cursor".to_string(),
            path: Some("/win/cursor".to_string()),
            last_sync_at: Some(old_ts),
            config: serde_json::json!({ "cursor": "old" }),
        };
        hub.upsert_source(&hub_source).await.unwrap();
        hub.close().await;

        let local_dir = tempfile::tempdir().unwrap();
        let local_path = local_dir.path().join("local.db");
        let local = Database::open(&local_path).await.unwrap();

        let local_source = Source {
            id: "cursor-abc".to_string(),
            adapter: "cursor".to_string(),
            path: Some("/win/cursor".to_string()),
            last_sync_at: Some(new_ts),
            config: serde_json::json!({ "cursor": "new" }),
        };
        local.upsert_source(&local_source).await.unwrap();
        local.close().await;

        let merged_dir = tempfile::tempdir().unwrap();
        let merged_path = merged_dir.path().join("merged.db");
        std::fs::copy(&hub_path, &merged_path).unwrap();
        let merged = Database::open(&merged_path).await.unwrap();

        let result = merge_databases(&merged, &local_path, "arknights")
            .await
            .unwrap();

        assert_eq!(result.sources_added, 0);
        assert_eq!(result.sources_updated, 1);

        let updated = merged
            .get_source("arknights:cursor-abc")
            .await
            .unwrap()
            .expect("source should exist");
        assert_eq!(
            updated.last_sync_at.map(|t| t.timestamp()),
            Some(new_ts.timestamp())
        );
        assert_eq!(updated.config["cursor"], "new");
    }

    #[tokio::test]
    async fn stale_push_does_not_regress_source_last_sync_at() {
        use crate::models::Source;

        let old_ts = Utc::now() - chrono::Duration::days(2);
        let new_ts = Utc::now();

        let hub_dir = tempfile::tempdir().unwrap();
        let hub_path = hub_dir.path().join("hub.db");
        let hub = Database::open(&hub_path).await.unwrap();

        let hub_source = Source {
            id: "arknights:cursor-abc".to_string(),
            adapter: "cursor".to_string(),
            path: Some("/win/cursor".to_string()),
            last_sync_at: Some(new_ts),
            config: serde_json::json!({ "cursor": "current" }),
        };
        hub.upsert_source(&hub_source).await.unwrap();
        hub.close().await;

        let local_dir = tempfile::tempdir().unwrap();
        let local_path = local_dir.path().join("local.db");
        let local = Database::open(&local_path).await.unwrap();

        let local_source = Source {
            id: "cursor-abc".to_string(),
            adapter: "cursor".to_string(),
            path: Some("/win/cursor-stale".to_string()),
            last_sync_at: Some(old_ts),
            config: serde_json::json!({ "cursor": "stale" }),
        };
        local.upsert_source(&local_source).await.unwrap();
        local.close().await;

        let merged_dir = tempfile::tempdir().unwrap();
        let merged_path = merged_dir.path().join("merged.db");
        std::fs::copy(&hub_path, &merged_path).unwrap();
        let merged = Database::open(&merged_path).await.unwrap();

        let result = merge_databases(&merged, &local_path, "arknights")
            .await
            .unwrap();

        assert_eq!(result.sources_added, 0);
        assert_eq!(result.sources_updated, 0);

        let unchanged = merged
            .get_source("arknights:cursor-abc")
            .await
            .unwrap()
            .expect("source should exist");
        assert_eq!(
            unchanged.last_sync_at.map(|t| t.timestamp()),
            Some(new_ts.timestamp())
        );
        assert_eq!(unchanged.config["cursor"], "current");
        assert_eq!(unchanged.path.as_deref(), Some("/win/cursor"));
    }

    #[tokio::test]
    async fn fetched_remote_must_be_a_valid_hstry_database() {
        let temp = tempfile::tempdir().expect("temp directory");
        let valid_path = temp.path().join("valid.db");
        Database::open(&valid_path)
            .await
            .expect("create database")
            .close()
            .await;
        validate_hstry_database(&valid_path)
            .await
            .expect("valid hstry database");

        // Reject non-sqlite file
        let invalid_path = temp.path().join("invalid.db");
        std::fs::write(&invalid_path, b"not a sqlite database").expect("write invalid database");
        let error = validate_hstry_database(&invalid_path)
            .await
            .expect_err("reject invalid database");
        assert!(matches!(error, Error::Remote(_)));

        // Reject sqlite file without conversations table
        let non_hstry_path = temp.path().join("non_hstry.db");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&non_hstry_path)
            .create_if_missing(true);
        let mut connection = sqlx::SqliteConnection::connect_with(&options)
            .await
            .expect("connect sqlite");
        sqlx::query("CREATE TABLE some_table (id INTEGER PRIMARY KEY);")
            .execute(&mut connection)
            .await
            .expect("create table");
        connection.close().await.expect("close connection");

        let error = validate_hstry_database(&non_hstry_path)
            .await
            .expect_err("reject non-hstry database without conversations table");
        assert!(matches!(error, Error::Remote(_)));
    }

    #[tokio::test]
    async fn empty_export_records_cut_without_requiring_remote_push() {
        let dir = tempfile::tempdir().unwrap();
        let src = Database::open(&dir.path().join("src.db")).await.unwrap();
        let delta_path = dir.path().join("delta.db");
        let export = export_delta(&src, &delta_path, Some(0)).await.unwrap();
        assert!(export.empty);
        assert_eq!(export.export_cut, 0);
        src.close().await;
    }

    #[test]
    fn fold_exact_duplicates_keeps_distinct_provenance() {
        use crate::models::{MessageRole, SearchHit};
        use crate::recall::{Provenance, fold_exact_duplicates};
        use uuid::Uuid;

        let mk = |source: &str, machine: Option<&str>, content: &str| SearchHit {
            message_id: Uuid::new_v4(),
            conversation_id: Uuid::new_v4(),
            message_idx: 0,
            role: MessageRole::User,
            content: content.into(),
            snippet: content.into(),
            match_position: Some(0),
            provenance: Provenance {
                source: source.into(),
                machine: machine.map(str::to_owned),
                completeness: "unknown".into(),
                ..Default::default()
            },
            created_at: None,
            conv_created_at: Utc::now(),
            conv_updated_at: None,
            score: 0.0,
            source_id: source.into(),
            external_id: None,
            readable_id: None,
            title: Some("same-title".into()),
            workspace: None,
            source_adapter: "pi".into(),
            source_path: None,
            host: machine.map(str::to_owned),
            conversation_version: Some(1),
            occurrences: None,
        };

        let mut hits = vec![
            mk("pi", Some("local"), "same body"),
            mk("pi", Some("local"), "same body"),
            mk("pi", Some("hub"), "same body"),
            mk("pi", Some("local"), "different body"),
        ];
        hits.push(hits[0].clone());
        fold_exact_duplicates(&mut hits);
        assert_eq!(hits.len(), 4);
    }

    #[test]
    fn combine_scoped_reports_keeps_other_side_on_partial_outage() {
        use crate::config::SearchScope;
        use crate::recall::{SearchReport, combine_scoped_reports};

        let local = SearchReport {
            hits: Vec::new(),
            scope: "local_snapshot".into(),
            warnings: Vec::new(),
            ..Default::default()
        };
        let report = combine_scoped_reports(
            SearchScope::All,
            &[],
            Some(Ok(local)),
            Some(Err(Error::Remote("hub down".into()))),
        )
        .expect("local should survive");
        assert!(report.warnings.iter().any(|w| w.contains("Coverage error")));
        assert_eq!(report.scope, "local_snapshot_and_remote");

        let err = combine_scoped_reports(
            SearchScope::All,
            &[],
            Some(Err(Error::Other("local down".into()))),
            Some(Err(Error::Remote("hub down".into()))),
        )
        .expect_err("both sides failing must error");
        assert!(err.to_string().contains("Coverage error") || err.to_string().contains("failed"));
        assert!(
            combine_scoped_reports(
                SearchScope::Local,
                &[],
                Some(Err(Error::Other("local down".into()))),
                None
            )
            .is_err()
        );
        assert!(
            combine_scoped_reports(
                SearchScope::Remote,
                &[],
                None,
                Some(Err(Error::Remote("hub down".into())))
            )
            .is_err()
        );
        assert!(
            combine_scoped_reports(
                SearchScope::Local,
                &[],
                Some(Ok(SearchReport::default())),
                None
            )
            .is_ok()
        );
        assert!(
            combine_scoped_reports(
                SearchScope::Remote,
                &[],
                None,
                Some(Ok(SearchReport::default()))
            )
            .is_ok()
        );
    }
}
