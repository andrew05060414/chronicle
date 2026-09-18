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
        let output = self.exec(&file_exists_command(remote_path))?;
        Ok(output.trim() == "yes")
    }

    /// Get the expanded path on the remote (resolves ~ and env vars).
    ///
    /// Expansion is limited to a leading `~` and portable `$VAR` / `${VAR}`
    /// names. Operators, globs, and command substitutions stay literal data.
    pub fn expand_remote_path(&self, path: &str) -> Result<String> {
        let output = self.exec(&expand_remote_path_command(path))?;
        Ok(output.trim().to_string())
    }
}
