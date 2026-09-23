use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::io::stdio,
};

use hstry_core::Config;

#[cfg(test)]
mod recall_tests;

async fn refresh_local_via_cli(config_path: Option<&PathBuf>) -> anyhow::Result<()> {
    use std::process::Stdio;
    use tokio::process::Command;
    let mut cmd = Command::new("hstry");
    cmd.arg("sync")
        .arg("--json")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(path) = config_path_hint(config_path) {
        cmd.arg("--config").arg(path);
    }
    let child = cmd.spawn()?;
    match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => Ok(()),
        Ok(Ok(output)) => Err(anyhow::anyhow!(
            "hstry sync failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )),
        Ok(Err(err)) => Err(err.into()),
        Err(_) => Err(anyhow::anyhow!("local refresh timed out after 5s")),
    }
}

fn config_path_hint(explicit: Option<&PathBuf>) -> Option<std::path::PathBuf> {
    // Best-effort: if the process was started with an explicit config path via
    // HSTRY_CONFIG, reuse it for the child sync. Otherwise let hstry resolve defaults.
    if let Some(path) = explicit {
        return Some(path.clone());
    }
    std::env::var_os("HSTRY_CONFIG").map(PathBuf::from)
}

fn main() {
    if let Err(err) = try_main() {
        let _ = writeln!(io::stderr(), "{err:?}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn try_main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli
        .common
        .config
        .unwrap_or_else(Config::default_config_path);
    let config = Config::ensure_at(&config_path)?;

    let db = hstry_core::Database::open(&config.database).await?;
    let server = McpServer::new(config, db, Some(config_path));
    let transport = stdio();

    server
        .serve(transport)
        .await
        .map_err(|e| anyhow::anyhow!("MCP server error: {e}"))?;

    Ok(())
}

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Chronicle MCP server - retrieval over the local conversation archive"
)]
struct Cli {
    #[command(flatten)]
    common: CommonOpts,
}

#[derive(Debug, Clone, Args)]
struct CommonOpts {
    /// Override the config file path
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SearchRequest {
    query: String,
    /// auto (default), needle, exact (literal), regex, natural, code, recent
    mode: Option<String>,
    source: Option<String>,
    workspace: Option<String>,
    role: Option<String>,
    model: Option<String>,
    harness: Option<String>,
    tag: Option<String>,
    after: Option<String>,
    before: Option<String>,
    remote: Option<String>,
    /// local, remote, or all. Default: local+configured hub when hub is set.
    scope: Option<String>,
    /// Refresh local collection before search (local only, <=5s, never remote push).
    refresh_local: Option<bool>,
    limit: Option<i64>,
    offset: Option<i64>,
    max_chars: Option<usize>,
    snippet_chars: Option<usize>,
    raw: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ExpandRequest {
    conversation_id: String,
    message_idx: Option<i32>,
    message_id: Option<String>,
    before: Option<usize>,
    after: Option<usize>,
    expand_interactions: Option<bool>,
    field: Option<String>,
    /// Offset into field records, distinct from the character offset.
    page_offset: Option<usize>,
    limit: Option<usize>,
    version: Option<i64>,
    /// Character offset within the anchored message (use match_position from search).
    offset: Option<usize>,
    max_chars: Option<usize>,
    remote: Option<String>,
}

#[derive(Clone)]
struct McpServer {
    config: Config,
    db: std::sync::Arc<hstry_core::Database>,
    tool_router: ToolRouter<Self>,
    config_path: Option<PathBuf>,
}

impl McpServer {
    fn new(config: Config, db: hstry_core::Database, config_path: Option<PathBuf>) -> Self {
        Self {
            config,
            db: std::sync::Arc::new(db),
            tool_router: Self::tool_router(),
            config_path,
        }
    }
}

#[tool_router]
impl McpServer {
    #[tool(
        description = "Find relevant evidence across all message roles. Default 3000-char envelope with snippets, message anchors, attempted modes and snapshot completeness. Exact is literal; auto/needle report regex/FTS fallback. Use expand for a matching passage."
    )]
    async fn search(&self, Parameters(req): Parameters<SearchRequest>) -> String {
        let result: anyhow::Result<serde_json::Value> = async {
            use hstry_core::{
                db::{SearchMode, SearchOptions},
                recall::{Budget, project},
            };
            let mode = match req.mode.as_deref().unwrap_or("auto") {
                "auto" => SearchMode::Auto,
                "needle" => SearchMode::Needle,
                "exact" => SearchMode::Exact,
                "regex" => SearchMode::Regex,
                "natural" => SearchMode::NaturalLanguage,
                "code" => SearchMode::Code,
                "recent" => SearchMode::Recent,
                _ => anyhow::bail!("Invalid search mode"),
            };
            let opts = SearchOptions {
                mode,
                limit: req.limit,
                offset: req.offset,
                source_id: req.source,
                workspace: req.workspace,
                role: req.role,
                model: req.model,
                harness: req.harness,
                tag: req.tag,
                after: req.after.map(|s| s.parse()).transpose()?,
                before: req.before.map(|s| s.parse()).transpose()?,
            };
            let mut refresh_warnings = Vec::new();
            if req.refresh_local.unwrap_or(false) {
                match refresh_local_via_cli(self.config_path.as_ref()).await {
                    Ok(()) => {}
                    Err(err) => refresh_warnings.push(format!(
                        "refresh-local warning: {err}; continuing with existing local snapshot"
                    )),
                }
            }

            let explicit_remote = req.remote.clone();
            let remotes = if let Some(name) = &explicit_remote {
                vec![name.clone()]
            } else {
                Vec::new()
            };
            let scope = if explicit_remote.is_some() {
                hstry_core::config::SearchScope::Remote
            } else {
                let explicit = match req.scope.as_deref() {
                    Some("local") => Some(hstry_core::config::SearchScope::Local),
                    Some("remote") => Some(hstry_core::config::SearchScope::Remote),
                    Some("all") => Some(hstry_core::config::SearchScope::All),
                    Some(_) => anyhow::bail!("Invalid search scope"),
                    None => None,
                };
                self.config.resolve_search_scope(explicit)
            };

            if let Some(offset) = req.offset
                && offset > 0
                && !matches!(scope, hstry_core::config::SearchScope::Local)
                && !(matches!(scope, hstry_core::config::SearchScope::Remote) && remotes.len() == 1)
            {
                anyhow::bail!(
                    "Pagination requires a local search or one named remote; page each store separately"
                );
            }

            let need_local = !matches!(scope, hstry_core::config::SearchScope::Remote);
            let need_remote = !matches!(scope, hstry_core::config::SearchScope::Local);
            let remote_list = if need_remote {
                Some(self.config.remotes_for_search(&remotes)?)
            } else {
                None
            };

            let local_fut = async {
                if need_local {
                    Some(self.db.search_report(&req.query, opts.clone()).await)
                } else {
                    None
                }
            };
            let remote_fut = async {
                if let Some(list) = remote_list.as_ref() {
                    if list.is_empty() || !list.iter().any(|r| r.enabled) {
                        Some(Err(hstry_core::Error::Other(
                            "No enabled remotes to search".into(),
                        )))
                    } else {
                        Some(hstry_core::remote::search_remotes(list, &req.query, &opts).await)
                    }
                } else {
                    None
                }
            };
            let (local, remote) = tokio::join!(local_fut, remote_fut);
            let mut report = hstry_core::recall::combine_scoped_reports(
                scope,
                &remotes,
                local,
                remote,
            )?;
            report.warnings.splice(0..0, refresh_warnings);
            report.available_remotes = self
                .config
                .remotes
                .iter()
                .filter(|r| r.enabled)
                .map(|r| r.name.clone())
                .collect();
            Ok(project(
                &report,
                Budget {
                    total: req.max_chars.unwrap_or(3000),
                    snippet: req.snippet_chars.unwrap_or(300),
                },
                req.raw.unwrap_or(false),
            )?)
        }
        .await;
        match result {
            Ok(v) => v.to_string(),
            Err(e) => {
                serde_json::json!({"ok":false,"error":hstry_core::recall::clip(&e.to_string(),300)})
                    .to_string()
            }
        }
    }

    #[tool(
        description = "Read evidence with a serialized budget locally or on a named remote. Pass message_idx or message_id for anchor-first context, or omit for chronological pages. Each record has a field and next_offset_chars: continue that field with offset and field. next_offset advances field-record pages via page_offset. Pass version during pagination to detect changes. expand_interactions includes only directly linked tool calls/results."
    )]
    async fn expand(&self, Parameters(req): Parameters<ExpandRequest>) -> String {
        let result: anyhow::Result<serde_json::Value> = async {
            let options = hstry_core::read::ReadOptions {
                message_idx: req.message_idx,
                message_id: req.message_id.as_deref().map(str::parse).transpose()?,
                before: req.before.unwrap_or(0),
                after: req.after.unwrap_or(0),
                expand_interactions: req.expand_interactions.unwrap_or(false),
                offset: req.page_offset.unwrap_or(0),
                limit: req.limit.unwrap_or(50),
                max_chars: req.max_chars.unwrap_or(3000),
                field: req.field.or_else(|| req.offset.map(|_| "content".into())),
                offset_chars: req.offset.unwrap_or(0),
                version: req.version,
                machine: None,
            };
            let page = if let Some(name) = &req.remote {
                let peer = self
                    .config
                    .remotes
                    .iter()
                    .find(|r| r.name == *name && r.enabled)
                    .ok_or_else(|| anyhow::anyhow!("Unknown remote"))?;
                hstry_core::remote::read_remote(peer, &req.conversation_id, &options).await?
            } else {
                self.db
                    .read_page(req.conversation_id.parse()?, options)
                    .await?
            };
            Ok(serde_json::json!({"ok":true,"result":page}))
        }
        .await;
        match result {
            Ok(v) => v.to_string(),
            Err(e) => {
                serde_json::json!({"ok":false,"error":hstry_core::recall::clip(&e.to_string(),300)})
                    .to_string()
            }
        }
    }

    /// Get service configuration
    #[tool(description = "Returns the service configuration (enabled and poll interval)")]
    async fn get_runtime_config(&self) -> String {
        tokio::task::yield_now().await;
        serde_json::to_string_pretty(&self.config.service).unwrap_or_else(|_| "{}".to_string())
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some(
                "Chronicle is a conversation archive over AI chat sessions already recorded \
                 on this machine and on its configured remotes. This MCP server exposes a \
                 retrieval-only tool surface: use `search` to locate evidence and `expand` \
                 to read a bounded window around a match, and no archive-mutating MCP tools \
                 are exposed; both return a snapshot of what was stored, never proof that \
                 something does not exist. Chronicle is not a task board and dispatches no \
                 work - it answers what was said, and nothing here assigns, schedules, or \
                 tracks tasks."
                    .to_string(),
            ),
            ..Default::default()
        }
    }
}
