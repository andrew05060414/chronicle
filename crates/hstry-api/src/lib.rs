use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use hstry_core::db::{SearchMode, SearchOptions};
use hstry_core::ingest::ingest_batch;
use hstry_core::models::Source;
use hstry_core::parsed::ParsedConversation;
use hstry_core::{Config, Database};

#[cfg(test)]
mod search_tests;

/// Ingest payloads carry full conversation histories; allow generous bodies.
pub const INGEST_BODY_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Arc<Database>,
    pub ingest_token: Arc<Option<String>>,
}

impl AppState {
    pub fn new(config: Arc<Config>, db: Arc<Database>, ingest_token: Option<String>) -> Self {
        Self {
            config,
            db,
            ingest_token: Arc::new(ingest_token),
        }
    }
}

pub fn create_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/config", get(get_config))
        .route("/search", get(search))
        .route("/read", post(read_evidence))
        .route("/sources", post(register_source))
        .route(
            "/ingest",
            post(ingest).layer(DefaultBodyLimit::max(INGEST_BODY_LIMIT)),
        )
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Start HTTP server listening on the specified port.
/// Returns the spawned background task join handle and bound local address.
pub async fn start_http_server(
    port: u16,
    state: AppState,
) -> Result<(tokio::task::JoinHandle<()>, SocketAddr)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    let app = create_router(state);

    let handle = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            log::error!("HTTP API error: {err}");
        }
    });

    Ok((handle, local_addr))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    pub id: String,
    #[serde(default)]
    pub options: hstry_core::read::ReadOptions,
    pub remote: Option<String>,
}

pub async fn read_evidence(
    State(state): State<AppState>,
    Json(req): Json<ReadRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let result: anyhow::Result<_> = async {
        if let Some(name) = req.remote {
            let peer = state
                .config
                .remotes
                .iter()
                .find(|r| r.name == name && r.enabled)
                .ok_or_else(|| anyhow::anyhow!("Unknown remote"))?;
            Ok(hstry_core::remote::read_remote(peer, &req.id, &req.options).await?)
        } else {
            Ok(state.db.read_page(req.id.parse()?, req.options).await?)
        }
    }
    .await;
    match result {
        Ok(page) => Ok(Json(serde_json::json!({"ok":true,"result":page}))),
        Err(err) => Err((
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({"ok":false,"error":hstry_core::recall::clip(&err.to_string(),300)}),
            ),
        )),
    }
}

#[derive(Serialize)]
pub struct RootResponse {
    pub name: &'static str,
    pub version: &'static str,
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
}

pub async fn root() -> Json<RootResponse> {
    Json(RootResponse {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
    })
}

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

pub async fn get_config(State(state): State<AppState>) -> Result<Json<Config>, StatusCode> {
    Ok(Json((*state.config).clone()))
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub query: String,
    pub max_chars: Option<usize>,
    pub snippet_chars: Option<usize>,
    pub raw: Option<bool>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub source: Option<String>,
    pub workspace: Option<String>,
    pub mode: Option<String>,
    /// ISO 8601 timestamp: only messages after this time
    pub after: Option<String>,
    /// ISO 8601 timestamp: only messages before this time
    pub before: Option<String>,
    /// Filter by message role
    pub role: Option<String>,
    /// Filter by conversation model
    pub model: Option<String>,
    /// Filter by agent harness
    pub harness: Option<String>,
    /// Filter by conversation tag
    pub tag: Option<String>,
}

pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mode = match params.mode.as_deref() {
        Some("auto") | None => SearchMode::Auto,
        Some("natural" | "natural_language") => SearchMode::NaturalLanguage,
        Some("code") => SearchMode::Code,
        Some("exact") => SearchMode::Exact,
        Some("needle") => SearchMode::Needle,
        Some("regex") => SearchMode::Regex,
        Some("recent") => SearchMode::Recent,
        _ => return Err(StatusCode::BAD_REQUEST),
    };

    let after = params
        .after
        .as_deref()
        .map(dateparser::parse)
        .transpose()
        .map_err(|_| StatusCode::BAD_REQUEST)?
        .map(|dt| dt.with_timezone(&chrono::Utc));
    let before = params
        .before
        .as_deref()
        .map(dateparser::parse)
        .transpose()
        .map_err(|_| StatusCode::BAD_REQUEST)?
        .map(|dt| dt.with_timezone(&chrono::Utc));

    let source = params.source.clone();
    let workspace = params.workspace.clone();
    let role = params.role.clone();
    let model = params.model.clone();
    let harness = params.harness.clone();
    let tag = params.tag.clone();

    let mut results = state
        .db
        .search_report(
            &params.query,
            SearchOptions {
                source_id: source,
                workspace,
                limit: params.limit,
                offset: params.offset,
                mode,
                after,
                before,
                role,
                model,
                harness,
                tag,
            },
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    results.available_remotes = state
        .config
        .remotes
        .iter()
        .filter(|r| r.enabled)
        .map(|r| r.name.clone())
        .collect();
    let budget = hstry_core::recall::Budget {
        total: params.max_chars.unwrap_or(3000),
        snippet: params.snippet_chars.unwrap_or(300),
    };
    let envelope = hstry_core::recall::project(&results, budget, params.raw.unwrap_or(false))
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok(Json(envelope))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestRequest {
    /// Source id the conversations belong to (created on first use).
    pub source: String,
    /// Adapter/provider label stored on a newly created source.
    pub adapter: Option<String>,
    pub conversations: Vec<ParsedConversation>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestResponse {
    pub source: String,
    pub conversations: usize,
    pub created: usize,
    pub updated: usize,
    pub messages: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterSourceRequest {
    pub source: String,
    pub adapter: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterSourceResponse {
    pub source: String,
    pub adapter: String,
    pub created: bool,
}

pub fn authorize_ingest(state: &AppState, headers: &HeaderMap) -> Result<(), StatusCode> {
    if let Some(expected) = state.ingest_token.as_ref() {
        let provided = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if provided != Some(expected.as_str()) {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    Ok(())
}

pub fn valid_source_id(source_id: &str) -> bool {
    !source_id.is_empty()
        && source_id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

pub async fn register_source(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RegisterSourceRequest>,
) -> Result<Json<RegisterSourceResponse>, StatusCode> {
    authorize_ingest(&state, &headers)?;
    let source_id = req.source.trim();
    let adapter = req.adapter.trim();
    if !valid_source_id(source_id) || adapter.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let existing = state
        .db
        .get_source(source_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let created = existing.is_none();
    let source = existing.unwrap_or_else(|| Source {
        id: source_id.to_string(),
        adapter: adapter.to_string(),
        path: None,
        last_sync_at: None,
        config: serde_json::json!({}),
    });
    state
        .db
        .upsert_source(&source)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(RegisterSourceResponse {
        source: source.id,
        adapter: source.adapter,
        created,
    }))
}

pub async fn ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<IngestRequest>,
) -> Result<Json<IngestResponse>, StatusCode> {
    authorize_ingest(&state, &headers)?;

    let source_id = req.source.trim();
    if !valid_source_id(source_id) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let mut source = state
        .db
        .get_source(source_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .unwrap_or_else(|| Source {
            id: source_id.to_string(),
            adapter: req.adapter.clone().unwrap_or_else(|| source_id.to_string()),
            path: None,
            last_sync_at: None,
            config: serde_json::json!({}),
        });

    let outcome = ingest_batch(&state.db, source_id, req.conversations)
        .await
        .map_err(|err| {
            log::error!("ingest failed for source '{source_id}': {err:?}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    if !outcome.affected_conversation_ids.is_empty() {
        state
            .db
            .rebuild_conversation_summaries(&outcome.affected_conversation_ids)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    source.last_sync_at = Some(Utc::now());
    state
        .db
        .upsert_source(&source)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(IngestResponse {
        source: source_id.to_string(),
        conversations: outcome.conversations,
        created: outcome.created,
        updated: outcome.updated,
        messages: outcome.messages,
    }))
}
