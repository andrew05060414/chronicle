
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::stream::{self, StreamExt};
use hstry_core::config::{AdapterRepo, AdapterRepoSource};
use hstry_core::models::{Conversation, Message, MessageRole, Source};
use hstry_core::{Config, Database};
use hstry_runtime::{AdapterRunner, ExportConversation, ExportOptions, ParsedMessage, Runtime};

/// Apply storage feature flags from `config` to a freshly opened `Database`.
/// Centralised so every entry point honours the trx-aa3m / trx-z42c contracts.
fn apply_storage_config(db: &Database, config: &Config) {
    db.set_message_events_enabled(config.storage.message_events.enabled);
    db.set_indexer_outbox_enabled(config.storage.indexer_outbox.enabled);
}

mod adapter_manifest;
use serde::{Serialize, de::DeserializeOwned};

mod backup;
mod pretty;
mod read_cli;
mod resume;
mod service;
mod skill;
mod skills;
mod sync;

#[derive(Debug, serde::Deserialize)]
struct SyncInput {
    source: Option<String>,
    parallel: Option<usize>,
}

#[derive(Debug, serde::Deserialize)]
struct SearchInput {
    query: String,
    limit: Option<i64>,
    source: Option<String>,
    workspace: Option<String>,
    mode: Option<SearchModeArg>,
    scope: Option<SearchScopeArg>,
    remotes: Option<Vec<String>>,
    offset: Option<i64>,
    after: Option<String>,
    before: Option<String>,
    role: Option<Vec<SearchRoleArg>>,
    model: Option<String>,
    harness_filter: Option<String>,
    tag: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ListInput {
    source: Option<String>,
    workspace: Option<String>,
    limit: Option<i64>,
    after: Option<String>,
    before: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ShowInput {
    id: String,
}

#[derive(Debug, serde::Deserialize)]
struct SourceAddInput {
    path: String,
    adapter: Option<String>,
    id: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct SourceRemoveInput {
    id: String,
}

#[derive(Debug, serde::Deserialize)]
struct AdapterAddInput {
    path: String,
}

#[derive(Debug, serde::Deserialize)]
struct AdapterToggleInput {
    name: String,
}

#[derive(Debug, serde::Serialize)]
struct JsonResponse<T> {
    ok: bool,
    result: Option<T>,
    error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct SyncSummary {
    sources: Vec<sync::SyncStats>,
    total_sources: usize,
    total_conversations: usize,
    total_messages: usize,
}

#[derive(Debug, serde::Serialize)]
struct StatsSummary {
    sources: i64,
    conversations: i64,
    messages: i64,
    per_source: Vec<hstry_core::db::SourceStats>,
    activity: hstry_core::db::ActivityStats,
}

#[derive(Debug, serde::Serialize)]
struct ScanHit {
    adapter: String,
    display_name: String,
    path: String,
    confidence: f32,
}

#[derive(Debug, serde::Serialize)]
struct AdapterStatus {
    name: String,
    enabled: bool,
}
#[derive(Debug, Parser)]
#[command(
    name = env!("CARGO_BIN_NAME"),
    author,
    version,
    about = "Chronicle connecting layer for hstry — universal AI chat history",
    propagate_version = true
)]
struct Cli {
    /// Config file path
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Output JSON for programmatic use
    #[arg(long, global = true)]
    json: bool,

    /// Disable colored output (also honors NO_COLOR)
    #[arg(long, global = true)]
    no_color: bool,

    /// Increase verbosity
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Install, inspect, or update the bundled agent retrieval skill
    Skill {
        #[command(subcommand)]
        command: skill::Command,
    },
    /// Sync chat history from all sources
    Sync {
        /// Only sync a specific source
        #[arg(long)]
        source: Option<String>,

        /// Max number of sources to sync in parallel
        #[arg(long)]
        parallel: Option<usize>,

        /// Read JSON input from file or "-" for stdin
        #[arg(long)]
        input: Option<PathBuf>,
    },

    /// Import chat history from a file or directory with auto-detection
    Import {
        /// Path to file or directory to import
        path: PathBuf,

        /// Force a specific adapter (skip auto-detection)
        #[arg(short, long)]
        adapter: Option<String>,

        /// Custom source ID (defaults to adapter name)
        #[arg(long)]
        source_id: Option<String>,

        /// Only show what would be imported (don't write to database)
        #[arg(long)]
        dry_run: bool,
    },

    /// Search across chat history
    Search {
        /// Search query (empty lists recent evidence)
        #[arg(default_value = "")]
        query: String,

        /// Hard total JSON character budget
        #[arg(long, default_value_t = 3000)]
        max_chars: usize,

        /// Maximum characters per evidence snippet (up to 300)
        #[arg(long, default_value_t = 300)]
        snippet_chars: usize,

        /// Lossless JSON report including full message content (unbudgeted)
        #[arg(long)]
        raw: bool,

        /// Write content-free retrieval diagnostics to a new local file (never queries or IDs)
        #[arg(long)]
        trace_file: Option<PathBuf>,

        /// Skip this many ranked matches
        #[arg(long, default_value_t = 0)]
        offset: i64,

        /// Maximum results
        #[arg(short, long, default_value = "20")]
        limit: i64,

        /// Filter by source
        #[arg(long)]
        source: Option<String>,

        /// Filter by workspace
        #[arg(long)]
        workspace: Option<String>,

        /// Search mode (auto, natural, code)
        #[arg(long, value_enum, default_value = "auto")]
        mode: SearchModeArg,

        /// Search scope (local, remote, all). Satellite + hub_remote defaults to all (local + hub).
        #[arg(long, value_enum)]
        scope: Option<SearchScopeArg>,

        /// Remote names to query (default: hub_remote if set, otherwise all enabled)
        #[arg(long)]
        remote: Vec<String>,

        /// Filter by message role (user, assistant, system, tool)
        #[arg(long, short = 'r', value_enum)]
        role: Vec<SearchRoleArg>,

        /// Exclude tool calls and tool results from results
        #[arg(long)]
        no_tools: bool,

        /// Deduplicate similar results (by content hash)
        #[arg(long)]
        dedup: bool,

        /// Include system context (AGENTS.md, etc.) in results
        #[arg(long)]
        include_system: bool,

        /// Only include messages after this date (ISO 8601 or relative: "2d", "1w", "2025-01-15")
        #[arg(long)]
        after: Option<String>,

        /// Only include messages before this date (ISO 8601 or relative)
        #[arg(long)]
        before: Option<String>,

        /// Filter by conversation model (e.g. "claude-sonnet-4")
        #[arg(long)]
        model: Option<String>,

        /// Filter by agent harness (e.g. "pi", "claude")
        #[arg(long)]
        harness_filter: Option<String>,

        /// Filter by conversation tag
        #[arg(long)]
        tag: Option<String>,

        /// Show each session only once with occurrence count
        #[arg(short, long)]
        compact: bool,

        /// Read JSON input from file or "-" for stdin
        #[arg(long)]
        input: Option<PathBuf>,
    },

    /// Build or refresh the search index
    Index {
        /// Rebuild the index from scratch
        #[arg(long)]
        rebuild: bool,
    },

    /// List conversations
    List {
        /// Filter by source
        #[arg(long)]
        source: Option<String>,

        /// Filter by workspace
        #[arg(long)]
        workspace: Option<String>,

        /// Maximum results
        #[arg(short, long, default_value = "50")]
        limit: i64,

        /// Only include conversations created after this date (ISO 8601 or relative: "2d", "1w", "2025-01-15")
        #[arg(long)]
        after: Option<String>,

        /// Only include conversations created before this date (ISO 8601 or relative)
        #[arg(long)]
        before: Option<String>,

        /// Read JSON input from file or "-" for stdin
        #[arg(long)]
        input: Option<PathBuf>,

        /// Attach a token-efficient peek bundle to each result (files touched,
        /// tool counts, last messages). Implies --json.
        #[arg(long)]
        peek: bool,

        /// Truncate `last_assistant` to N chars in peek bundles (default 400).
        #[arg(long, requires = "peek")]
        peek_chars: Option<usize>,

        /// Include continuation fragments (resume/compaction sessions) that are
        /// hidden by default. Their content is always searchable regardless.
        #[arg(short, long)]
        all: bool,
    },

    /// Read budgeted evidence pages locally or from a named SSH source
    Read {
        /// Conversation ID (optional when --input supplies it)
        id: Option<String>,
        #[arg(long)]
        input: Option<PathBuf>,
        #[arg(long)]
        remote: Option<String>,
        #[command(flatten)]
        options: read_cli::ReadArgs,
    },

    /// Show a bounded conversation page; use read for continuation controls
    Show {
        /// Explicitly return the complete, unbounded legacy transcript
        #[arg(long)]
        full: bool,
        /// Read only the anchored message from search output
        #[arg(long)]
        message_idx: Option<i32>,

        /// Conversation ID, unique prefix, or external ID
        id: Option<String>,

        /// Read JSON input from file or "-" for stdin
        #[arg(long)]
        input: Option<PathBuf>,
    },

    /// Show a token-efficient peek bundle for a single conversation
    Peek {
        /// Conversation ID, unique prefix, or external ID
        id: String,

        /// Truncate `last_assistant` to N chars (default 400).
        #[arg(long)]
        chars: Option<usize>,
    },

    /// Remove one conversation and its related data
    Remove {
        /// Conversation UUID, unique prefix, or external ID
        id: String,

        /// Delete without prompting; otherwise only preview the removal
        #[arg(long)]
        yes: bool,

        /// Preview the removal without changing the database
        #[arg(long, conflicts_with = "yes")]
        dry_run: bool,
    },

    /// Manage sources
    Source {
        #[command(subcommand)]
        command: SourceCommand,
    },

    /// Manage adapters
    Adapters {
        #[command(subcommand)]
        command: Option<AdapterCommand>,
    },

    /// Manage background service
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },

    /// Scan for chat history sources
    Scan,

    /// Quickstart: scan, add sources, and sync
    Quickstart,

    /// Export conversations to another format
    Export {
        /// Target format (pi, opencode, codex, claude-code, markdown, json)
        #[arg(short, long)]
        format: String,

        /// Conversation IDs to export (comma-separated, or "all" for all)
        #[arg(short, long, default_value = "all")]
        conversations: String,

        /// Filter by source
        #[arg(long)]
        source: Option<String>,

        /// Filter by workspace
        #[arg(long)]
        workspace: Option<String>,

        /// Filter by message role (user, assistant, system, tool)
        #[arg(long, short = 'r', value_enum)]
        role: Vec<SearchRoleArg>,

        /// Output path (file for single-output exports, directory for multi-file exports)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Export one file per conversation for markdown/json
        #[arg(long)]
        session_files: bool,

        /// Pretty print JSON output
        #[arg(long)]
        pretty: bool,
    },

    /// Resume a conversation in a coding agent
    ///
    /// Opens a past session in your preferred agent (pi, claude-code, codex, etc.).
    /// If the session is already from the target agent, opens it directly.
    /// Otherwise, converts to the target format and places it in the agent's
    /// native session directory.
    Resume {
        /// Conversation ID (UUID or partial match)
        #[arg(group = "target")]
        id: Option<String>,

        /// Search for a conversation instead of specifying an ID
        #[arg(short, long, group = "target")]
        search: Option<String>,

        /// Target agent to resume in (default: from config)
        #[arg(short, long)]
        agent: Option<String>,

        /// Filter by source
        #[arg(long)]
        source: Option<String>,

        /// Filter by workspace
        #[arg(long)]
        workspace: Option<String>,

        /// Only show conversations after this date/time (natural language: "yesterday", "2 days ago", "2026-03-01")
        #[arg(long)]
        after: Option<String>,

        /// Only show conversations before this date/time (natural language)
        #[arg(long)]
        before: Option<String>,

        /// Maximum results when searching
        #[arg(short, long, default_value = "20")]
        limit: i64,

        /// Show what would happen without writing or launching
        #[arg(long)]
        dry_run: bool,

        /// Explicitly allow launching a converted transcript without verified native compatibility
        #[arg(long)]
        allow_unverified: bool,

        /// Interactive picker using fzf
        #[arg(short, long)]
        pick: bool,
    },

    /// Show database statistics
    Stats,

    /// Deduplicate conversations in the database
    Dedup {
        /// Only show what would be deleted (don't actually delete)
        #[arg(long)]
        dry_run: bool,

        /// Filter by source
        #[arg(long)]
        source: Option<String>,

        /// Deduplicate across sources by harness + external_id (e.g. multiple Cursor paths)
        #[arg(long)]
        cross_source: bool,
    },

    /// Integrate with mmry
    Mmry {
        #[command(subcommand)]
        command: MmryCommand,
    },

    /// Manage remote hosts for syncing history
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },

    /// Hub-side operations (ingest a satellite delta into the live archive)
    Hub {
        #[command(subcommand)]
        command: HubCommand,
    },

    /// Manage rolling hub checkpoints
    Checkpoint {
        #[command(subcommand)]
        command: CheckpointCommand,
    },

    /// Manage web-app automation
    Web {
        #[command(subcommand)]
        command: WebCommand,
    },

    /// Show or manage configuration
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },

    /// Rebuild a source from scratch (purge → import → [dedup] → index).
    ///
    /// Use this when a source's database state has drifted from its on-disk
    /// JSONL files (duplicate replays, partial imports, schema regressions).
    /// The default uses bulk-reseed mode for throughput; pass `--no-bulk` to
    /// keep indexes online.
    ///
    /// Dedup is *off* by default for reseed: with stable, content-addressable
    /// message ids in place (trx-hjjw.4), re-imports are naturally idempotent
    /// via ON CONFLICT and the conversation-local dedup heuristic is not
    /// needed. Pass `--dedup` to opt in for legacy cleanups.
    Reseed {
        /// Source ID to reseed (required — reseed never touches everything).
        #[arg(long)]
        source: String,
        /// Run conversation-local dedup after import (legacy cleanup).
        #[arg(long)]
        dedup: bool,
        /// Skip the post-import index rebuild.
        #[arg(long)]
        no_index: bool,
        /// Disable bulk reseed mode (keeps indexes online).
        #[arg(long)]
        no_bulk: bool,
        /// Don't actually purge / import, just print the plan.
        #[arg(long)]
        dry_run: bool,
        /// Also drop the source row itself when purging.
        #[arg(long)]
        drop_source: bool,
    },

    /// Verify that a source's DB state matches its on-disk artifacts.
    ///
    /// For Pi (and any adapter that exposes a stable per-conversation
    /// idempotency key) this re-parses the JSONL files, compares conversation
    /// counts and message counts to the database, and reports the drift.
    /// Pass `--repair` to run a `reseed` for any source that has drifted.
    Verify {
        /// Source ID to verify (defaults to all enabled sources).
        #[arg(long)]
        source: Option<String>,
        /// Reseed any drifted source automatically.
        #[arg(long)]
        repair: bool,
    },

    /// 3-2-1 backup of the live archive (NAS + Oracle + Google Drive)
    Backup {
        /// Print planned paths without writing or transferring
        #[arg(long)]
        dry_run: bool,

        /// Destinations (repeatable). `all` = nas + oracle + gdrive
        #[arg(long, value_enum, default_value = "all")]
        target: Vec<backup::BackupTarget>,

        /// Encrypt the offsite snapshot. Requires CHRONICLE_BACKUP_KEY; no default passphrase.
        #[arg(long)]
        encrypt: bool,

        /// NAS remote name for `remote sync -d push`
        #[arg(long, default_value = "nas-lan")]
        nas_remote: String,
    },

    /// Proxy to Andrew-Skill / ASM (not a memory store)
    Skills {
        #[command(subcommand)]
        command: skills::SkillsCommand,
    },

    /// Open the terminal UI (`chronicle-tui` / `hstry-tui`)
    Tui,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Show current configuration
    Show,

    /// Show config file path
    Path,

    /// Open config in editor
    Edit,
}

#[derive(Debug, Subcommand)]
enum WebCommand {
    /// Install Playwright browsers for web automation
    Install {
        /// Browser to install (chromium, firefox, webkit)
        #[arg(long, default_value = "chromium")]
        browser: String,
    },

    /// Login to a web provider and store session state
    Login {
        /// Provider name (chatgpt, claude, gemini)
        provider: String,

        /// Run in headful mode
        #[arg(long)]
        headful: bool,

        /// Browser to use (chromium, firefox, webkit)
        #[arg(long)]
        browser: Option<String>,
    },

    /// Sync web providers and import chats
    Sync {
        /// Provider name (chatgpt, claude, gemini)
        #[arg(long)]
        provider: Option<String>,

        /// Run in headful mode
        #[arg(long)]
        headful: bool,

        /// Browser to use (chromium, firefox, webkit)
        #[arg(long)]
        browser: Option<String>,
    },

    /// Show web login and sync status
    Status,
}

#[derive(Debug, Subcommand)]
enum RemoteCommand {
    /// List configured remotes
    List,

    /// Add a remote host
    Add {
        /// Unique name for this remote
        name: String,

        /// SSH host (e.g., "user@hostname" or SSH config alias)
        host: String,

        /// Path to hstry database on remote
        #[arg(long)]
        database_path: Option<String>,

        /// SSH port (default: 22)
        #[arg(short, long)]
        port: Option<u16>,

        /// Path to SSH identity file
        #[arg(short, long)]
        identity_file: Option<String>,
    },

    /// Remove a remote host
    Remove {
        /// Remote name
        name: String,
    },

    /// Test connection to a remote
    Test {
        /// Remote name
        name: String,
    },

    /// Fetch remote database to local cache
    Fetch {
        /// Remote name (fetches all enabled remotes if not specified)
        #[arg(short, long)]
        remote: Option<String>,
    },

    /// Sync history with remote (fetch + merge)
    Sync {
        /// Remote name (syncs all enabled remotes if not specified)
        #[arg(short, long)]
        remote: Option<String>,

        /// Sync direction
        #[arg(short, long, value_enum, default_value = "pull")]
        direction: SyncDirectionArg,

        /// Fetch the whole hub, merge locally, and replace the remote file.
        /// Dangerous while the hub service is running; use only for recovery.
        #[arg(long)]
        full: bool,
    },

    /// Show remote cache status
    Status,
}

#[derive(Debug, Subcommand)]
enum HubCommand {
    /// Merge a satellite delta sqlite into the live hub database
    Ingest {
        /// Path to the delta sqlite on this machine
        #[arg(long)]
        file: PathBuf,

        /// Device namespace (arknights, macbook, ...)
        #[arg(long)]
        namespace: String,

        /// Delete the delta file after a successful ingest
        #[arg(long)]
        delete: bool,
    },
}

#[derive(Debug, Subcommand)]
enum CheckpointCommand {
    /// Create a compressed checkpoint of the live database
    Create {
        /// Tag this checkpoint as weekly (otherwise Sunday creates are tagged)
        #[arg(long)]
        weekly: bool,
    },

    /// List checkpoints
    List,

    /// Restore a checkpoint to a search-only path (never staging.db)
    Restore {
        /// Checkpoint stem (from `hstry checkpoint list`)
        stem: String,

        /// Destination sqlite path
        #[arg(long)]
        output: Option<PathBuf>,

        /// Replace the live hub database (stop the service first)
        #[arg(long)]
        live: bool,
    },

    /// Delete old checkpoints to stay under the size cap
    Prune,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum SyncDirectionArg {
    /// Pull from remote to local
    Pull,
    /// Push from local to remote
    Push,
    /// Bidirectional merge
    Bidirectional,
}

impl From<SyncDirectionArg> for hstry_core::remote::SyncDirection {
    fn from(value: SyncDirectionArg) -> Self {
        match value {
            SyncDirectionArg::Pull => hstry_core::remote::SyncDirection::Pull,
            SyncDirectionArg::Push => hstry_core::remote::SyncDirection::Push,
            SyncDirectionArg::Bidirectional => hstry_core::remote::SyncDirection::Bidirectional,
        }
    }
}

#[derive(Debug, Subcommand)]
enum SourceCommand {
    /// Add a new source
    Add {
        /// Path to source data
        path: PathBuf,

        /// Adapter to use (auto-detect if not specified)
        #[arg(long)]
        adapter: Option<String>,

        /// Custom source ID
        #[arg(long)]
        id: Option<String>,

        /// Read JSON input from file or "-" for stdin
        #[arg(long)]
        input: Option<PathBuf>,
    },

    /// List configured sources
    List,

    /// Remove a source
    Remove {
        /// Source ID
        id: String,

        /// Read JSON input from file or "-" for stdin
        #[arg(long)]
        input: Option<PathBuf>,
    },

    /// Clean up duplicate sources (same adapter/path with different IDs)
    Cleanup {
        /// Remove duplicate sources automatically
        #[arg(long)]
        auto_remove: bool,
    },

    /// Remove redundant Cursor sources (keep globalStorage when present)
    PruneCursor {
        /// Only show what would be removed
        #[arg(long)]
        dry_run: bool,

        /// Remove redundant sources automatically
        #[arg(long)]
        auto_remove: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AdapterCommand {
    /// List available adapters
    List,

    /// Add an adapter directory to the config
    Add {
        /// Path to the adapter directory
        path: PathBuf,

        /// Read JSON input from file or "-" for stdin
        #[arg(long)]
        input: Option<PathBuf>,
    },

    /// Enable an adapter for imports
    Enable {
        /// Adapter name
        name: String,

        /// Read JSON input from file or "-" for stdin
        #[arg(long)]
        input: Option<PathBuf>,
    },

    /// Disable an adapter for imports
    Disable {
        /// Adapter name
        name: String,

        /// Read JSON input from file or "-" for stdin
        #[arg(long)]
        input: Option<PathBuf>,
    },

    /// Update/download adapters from configured repositories
    Update {
        /// Specific adapter to update (updates all if not specified)
        #[arg(short, long)]
        adapter: Option<String>,

        /// Only update from specific repo
        #[arg(short, long)]
        repo: Option<String>,

        /// Force update even if already up to date
        #[arg(short, long)]
        force: bool,
    },

    /// Manage adapter repositories
    Repo {
        #[command(subcommand)]
        command: AdapterRepoCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AdapterRepoCommand {
    /// List configured adapter repositories
    List,

    /// Add a git repository (GitHub, GitLab, Gitea, self-hosted, etc.)
    AddGit {
        /// Repository name (e.g., "community")
        name: String,

        /// Git repository URL (HTTPS or SSH)
        url: String,

        /// Branch, tag, or commit to use
        #[arg(short = 'r', long, default_value = "main")]
        git_ref: String,

        /// Path within repo where adapters are located
        #[arg(short, long, default_value = "adapters")]
        path: String,
    },

    /// Add an archive URL (tarball or zip)
    AddArchive {
        /// Repository name
        name: String,

        /// URL to the archive (.tar.gz, .zip, .tgz)
        url: String,

        /// Path within archive where adapters are located
        #[arg(short, long, default_value = "adapters")]
        path: String,
    },

    /// Add a local filesystem path
    AddLocal {
        /// Repository name
        name: String,

        /// Path to adapters directory
        path: PathBuf,
    },

    /// Remove an adapter repository
    Remove {
        /// Repository name
        name: String,
    },

    /// Enable an adapter repository
    Enable {
        /// Repository name
        name: String,
    },

    /// Disable an adapter repository
    Disable {
        /// Repository name
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    /// Enable the service in config
    Enable,

    /// Disable the service in config
    Disable,

    /// Start the background service
    Start,

    /// Run the service in the foreground
    Run,

    /// Restart the background service
    Restart,

    /// Stop the background service
    Stop,

    /// Show service status
    Status,
}

#[derive(Debug, Subcommand)]
enum MmryCommand {
    /// Extract memories into mmry
    Extract {
        /// mmry store name
        #[arg(long, default_value = "hstry")]
        store: String,

        /// Path to mmry binary
        #[arg(long, default_value = "mmry")]
        mmry_bin: String,

        /// mmry config file path
        #[arg(long, value_name = "PATH")]
        mmry_config: Option<PathBuf>,

        /// Filter by source
        #[arg(long)]
        source: Option<String>,

        /// Filter by workspace
        #[arg(long)]
        workspace: Option<String>,

        /// Only include conversations created after this RFC3339 timestamp
        #[arg(long)]
        after: Option<String>,

        /// Limit number of conversations
        #[arg(long)]
        limit: Option<i64>,

        /// Include only these message roles (defaults: user, assistant)
        #[arg(long, value_enum)]
        role: Vec<MmryRoleArg>,

        /// Override memory type for all entries
        #[arg(long, value_enum)]
        memory_type: Option<MmryMemoryTypeArg>,

        /// Print payload instead of invoking mmry
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum MmryRoleArg {
    User,
    Assistant,
    System,
    Tool,
    Other,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum MmryMemoryTypeArg {
    Episodic,
    Semantic,
    Procedural,
}

fn default_log_filter(verbose: u8) -> &'static str {
    match verbose {
        0 => "warn,hstry_cli=info,hstry_core=info,sqlx=error",
        1 => "warn,hstry_cli=debug,hstry_core=debug,sqlx=warn",
        _ => "trace",
    }
}
