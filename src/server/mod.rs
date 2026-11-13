//! HTTP/SSE server for editor integrations, remote clients, and CI.
//!
//! The server is intentionally thin: every turn is executed by the same agent
//! loop as the CLI and TUI. Interactive decisions are carried over a side
//! channel. An SSE client receives a `permission_requested` event containing a
//! request id, then answers with `POST /api/permissions/{id}`. Questions use the
//! same pattern under `/api/questions/{id}`.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent::{self, RunOutcome};
use crate::bootstrap::Services;
use crate::checkpoint::Checkpointer;
use crate::providers::all_known_models;
use crate::session::{Session, user_message};
use crate::types::{
    AgentEvent, ImageAttachment, OutputFormat, PermissionDecision, PermissionMode,
    PermissionRequest, UiGateway, Usage, UserAnswer, UserQuestion,
};
use crate::ui::headless::{HeadlessGateway, HeadlessOptions};

const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_SESSION_LIMIT: usize = 50;
const MAX_SESSION_LIMIT: usize = 500;

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub host: String,
    pub port: u16,
    pub token: Option<String>,
    pub cors: bool,
}

type PendingPermissions = Arc<StdMutex<HashMap<String, oneshot::Sender<PermissionDecision>>>>;
type PendingQuestions = Arc<StdMutex<HashMap<String, oneshot::Sender<Option<UserAnswer>>>>>;
type ActiveMap = Arc<StdMutex<HashMap<String, CancellationToken>>>;

#[derive(Clone)]
struct AppState {
    services: Arc<Services>,
    token: Option<Arc<str>>,
    cors: bool,
    pending_permissions: PendingPermissions,
    pending_questions: PendingQuestions,
    active: ActiveMap,
    /// Sessions whose first turn has already run in this server process. A
    /// session loaded from disk (for example after a server restart) refreshes
    /// its codebase context on that first turn.
    seen_sessions: Arc<StdMutex<HashSet<String>>>,
    started: Instant,
}

impl AppState {
    fn new(services: Services, token: Option<String>, cors: bool) -> Self {
        let token = token
            .filter(|value| !value.trim().is_empty())
            .map(Arc::from);
        Self {
            services: Arc::new(services),
            token,
            cors,
            pending_permissions: Arc::new(StdMutex::new(HashMap::new())),
            pending_questions: Arc::new(StdMutex::new(HashMap::new())),
            active: Arc::new(StdMutex::new(HashMap::new())),
            seen_sessions: Arc::new(StdMutex::new(HashSet::new())),
            started: Instant::now(),
        }
    }

    fn activate(&self, id: &str, cancel: CancellationToken) -> Result<ActiveTurn, ApiError> {
        let mut active = lock(&self.active);
        if active.contains_key(id) {
            return Err(ApiError::conflict(format!(
                "session `{id}` already has an active turn"
            )));
        }
        active.insert(id.to_string(), cancel);
        Ok(ActiveTurn {
            active: self.active.clone(),
            id: id.to_string(),
        })
    }

    fn cancel_token(&self, id: &str) -> Option<CancellationToken> {
        lock(&self.active).get(id).cloned()
    }
}

struct ActiveTurn {
    active: ActiveMap,
    id: String,
}

impl Drop for ActiveTurn {
    fn drop(&mut self) {
        lock(&self.active).remove(&self.id);
    }
}

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A permission or question request waiting for a side-channel answer.
struct ServerGateway {
    events: mpsc::UnboundedSender<ServerEvent>,
    pending_permissions: PendingPermissions,
    pending_questions: PendingQuestions,
    issued_permissions: StdMutex<HashSet<String>>,
    issued_questions: StdMutex<HashSet<String>>,
    cancel: CancellationToken,
    auto_approve: bool,
}

impl ServerGateway {
    fn send_event(&self, event: AgentEvent) {
        let _ = self.events.send(ServerEvent::Agent(event));
    }
}

#[async_trait::async_trait]
impl UiGateway for ServerGateway {
    async fn authorize(&self, mut request: PermissionRequest) -> PermissionDecision {
        if self.auto_approve {
            return PermissionDecision::Allow;
        }
        if request.id.trim().is_empty() {
            request.id = Uuid::new_v4().to_string();
        }
        let id = request.id.clone();
        let (sender, receiver) = oneshot::channel();
        lock(&self.pending_permissions).insert(id.clone(), sender);
        lock(&self.issued_permissions).insert(id.clone());
        self.send_event(AgentEvent::PermissionRequested {
            request: request.clone(),
        });

        let decision = tokio::select! {
            result = receiver => result.unwrap_or_else(|_| PermissionDecision::Deny {
                reason: Some("the server dropped this permission request".into()),
            }),
            () = self.cancel.cancelled() => PermissionDecision::Deny {
                reason: Some("the turn was cancelled".into()),
            },
        };

        lock(&self.pending_permissions).remove(&id);
        lock(&self.issued_permissions).remove(&id);
        self.send_event(AgentEvent::PermissionResolved {
            id,
            allowed: decision.is_allowed(),
        });
        decision
    }

    async fn ask(&self, question: UserQuestion) -> Option<UserAnswer> {
        let id = Uuid::new_v4().to_string();
        let (sender, receiver) = oneshot::channel();
        lock(&self.pending_questions).insert(id.clone(), sender);
        lock(&self.issued_questions).insert(id.clone());
        self.send_event(AgentEvent::QuestionRequested {
            id: id.clone(),
            question,
        });

        let answer = tokio::select! {
            result = receiver => result.ok().flatten(),
            () = self.cancel.cancelled() => None,
        };

        lock(&self.pending_questions).remove(&id);
        lock(&self.issued_questions).remove(&id);
        self.send_event(AgentEvent::QuestionResolved {
            id,
            answer: answer.as_ref().map(UserAnswer::as_text),
        });
        answer
    }

    fn emit(&self, event: AgentEvent) {
        self.send_event(event);
    }
}

impl Drop for ServerGateway {
    fn drop(&mut self) {
        let issued_permissions = lock(&self.issued_permissions);
        let mut pending_permissions = lock(&self.pending_permissions);
        for id in issued_permissions.iter() {
            pending_permissions.remove(id);
        }
        let issued_questions = lock(&self.issued_questions);
        let mut pending_questions = lock(&self.pending_questions);
        for id in issued_questions.iter() {
            pending_questions.remove(id);
        }
    }
}

enum ServerEvent {
    Agent(AgentEvent),
    Done(TurnResult),
}

#[derive(Debug, Serialize)]
struct TurnResult {
    #[serde(rename = "type")]
    kind: &'static str,
    session_id: String,
    turn: u64,
    text: String,
    stop_reason: String,
    usage: Usage,
    cost_usd: f64,
    session_usage: Usage,
    session_cost_usd: f64,
    is_error: bool,
    cancelled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": self.message,
                "status": self.status.as_u16(),
            })),
        )
            .into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self::internal(format!("{error:#}"))
    }
}

#[derive(Debug, Default, Deserialize)]
struct ListSessionsQuery {
    #[serde(default)]
    limit: Option<usize>,
    /// Only return sessions created for this working directory.
    #[serde(default)]
    cwd: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
struct CreateSessionRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    permission_mode: Option<PermissionMode>,
}

#[derive(Debug, Default, Deserialize)]
struct UpdateSessionRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    permission_mode: Option<PermissionMode>,
}

#[derive(Debug, Default, Deserialize)]
struct ForkSessionRequest {
    #[serde(default)]
    name: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct TurnRequest {
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    images: Vec<ImageAttachment>,
    #[serde(default = "default_true")]
    stream: bool,
    /// Auto-approve permission prompts. Streaming clients normally leave this
    /// off and answer `permission_requested` events instead.
    #[serde(default)]
    auto_approve: bool,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    permission_mode: Option<PermissionMode>,
}

#[derive(Debug, Default, Deserialize)]
struct McpDiscoverRequest {
    #[serde(default)]
    server: Option<String>,
}

/// Start the HTTP/SSE server. The future resolves after a graceful shutdown.
pub async fn serve(services: Services, options: ServeOptions) -> Result<()> {
    let state = AppState::new(services, options.token.clone(), options.cors);
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind((options.host.as_str(), options.port))
        .await
        .with_context(|| {
            format!(
                "could not bind {}:{} (is another server already running?)",
                options.host, options.port
            )
        })?;
    let address = listener
        .local_addr()
        .context("could not determine the server address")?;

    eprintln!("Aaykra Code server listening on http://{address}");
    eprintln!("  API:    http://{address}/api");
    eprintln!("  Editor: http://{address}/v1");
    eprintln!("  Health: http://{address}/health");
    if state.token.is_some() {
        eprintln!("  Auth:   bearer token required");
    } else {
        eprintln!("  Auth:   disabled (pass --token to require a bearer token)");
        if !is_loopback(&options.host) {
            eprintln!(
                "  warning: the server is bound to a non-loopback address without authentication"
            );
        }
    }
    for warning in &state.services.warnings {
        eprintln!("warning: {warning}");
    }

    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("the HTTP server stopped unexpectedly");
    state.services.mcp.shutdown().await;
    result
}

fn is_loopback(host: &str) -> bool {
    host == "127.0.0.1" || host == "::1" || host == "localhost" || host.starts_with("127.")
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/", get(api_index))
        .route("/health", get(health))
        .route("/info", get(info))
        .route("/config", get(config_info))
        .route("/models", get(models))
        .route("/agents", get(list_agents))
        .route("/agents/{name}", get(get_agent))
        .route("/skills", get(list_skills))
        .route("/skills/{name}", get(get_skill))
        .route("/sessions", get(list_sessions).post(create_session))
        .route(
            "/sessions/{reference}",
            get(get_session)
                .patch(update_session)
                .delete(delete_session),
        )
        .route("/sessions/{reference}/fork", post(fork_session))
        .route("/sessions/{reference}/turn", post(start_turn))
        .route("/sessions/{reference}/turns", post(start_turn))
        .route("/sessions/{reference}/cancel", post(cancel_turn))
        .route("/sessions/{reference}/undo", post(undo_turn))
        .route("/permissions/{id}", post(answer_permission))
        .route("/questions/{id}", post(answer_question))
        .route("/mcp", get(mcp_info))
        .route("/mcp/discover", post(mcp_discover))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    let mut router = Router::new()
        .route("/", get(api_index))
        .route("/health", get(health))
        .nest("/api", api.clone())
        .nest("/v1", api)
        .with_state(state.clone());
    if state.cors {
        router = router.layer(middleware::from_fn(cors_middleware));
    }
    router
}

async fn auth_middleware(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if request.method() == Method::OPTIONS {
        return next.run(request).await;
    }
    if let Some(expected) = &state.token
        && !request_has_token(request.headers(), expected)
    {
        return ApiError::new(
            StatusCode::UNAUTHORIZED,
            "missing or invalid bearer token (send Authorization: Bearer <token>)",
        )
        .into_response();
    }
    next.run(request).await
}

fn request_has_token(headers: &HeaderMap, expected: &str) -> bool {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.trim().split_once(' ')?;
            scheme
                .eq_ignore_ascii_case("bearer")
                .then_some(token.trim())
        });
    let direct = headers
        .get("x-aaykra-token")
        .and_then(|value| value.to_str().ok())
        .map(str::trim);
    bearer
        .or(direct)
        .is_some_and(|supplied| constant_time_eq(supplied.as_bytes(), expected.as_bytes()))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

async fn cors_middleware(request: Request, next: Next) -> Response {
    let mut response = if request.method() == Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PATCH, DELETE, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization, content-type, x-aaykra-token"),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("86400"),
    );
    response
}

async fn api_index(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "name": "aaykra-code",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Aaykra Code server — REST + SSE",
        "auth_required": state.token.is_some(),
        "streaming": {
            "turn": "POST /api/sessions/{id}/turns with {\"prompt\":\"…\",\"stream\":true}",
            "events": "text_delta, thinking_delta, tool_started, tool_finished, notice, warning, error, todo_updated, usage, turn_started, turn_finished, permission_requested, permission_resolved, question_requested, question_resolved, result",
            "permissions": "answer permission_requested with POST /api/permissions/{id}",
            "questions": "answer question_requested with POST /api/questions/{id}"
        },
        "endpoints": [
            "GET /api/health",
            "GET /api/info",
            "GET /api/config",
            "GET /api/models",
            "GET /api/agents",
            "GET /api/skills",
            "GET /api/sessions",
            "POST /api/sessions",
            "GET /api/sessions/{id}",
            "PATCH /api/sessions/{id}",
            "DELETE /api/sessions/{id}",
            "POST /api/sessions/{id}/fork",
            "POST /api/sessions/{id}/turns",
            "POST /api/sessions/{id}/cancel",
            "POST /api/sessions/{id}/undo",
            "POST /api/permissions/{id}",
            "POST /api/questions/{id}",
            "GET /api/mcp",
            "POST /api/mcp/discover"
        ]
    }))
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "name": "aaykra-code",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": state.started.elapsed().as_secs(),
    }))
}

async fn info(State(state): State<AppState>) -> Json<Value> {
    let services = &state.services;
    let config = &services.config;
    let default_model = crate::models::resolve_model(config, None, None)
        .ok()
        .map(|resolved| {
            json!({
                "provider": resolved.reference.provider,
                "model": resolved.reference.model,
            })
        });
    let sessions = services.store.list().map(|list| list.len()).unwrap_or(0);
    Json(json!({
        "name": "aaykra-code",
        "version": env!("CARGO_PKG_VERSION"),
        "cwd": services.cwd,
        "config_files": services.config_files,
        "warnings": services.warnings,
        "default_model": default_model,
        "defaults": {
            "provider": config.default_provider,
            "model": config.model,
            "agent": config.default_agent,
            "permission_mode": config.permission_mode,
            "effort": config.effort,
        },
        "providers": config.providers.keys().collect::<Vec<_>>(),
        "sessions": sessions,
        "agents": services.agents.all().len(),
        "skills": services.skills.all().len(),
        "mcp_servers": services.mcp.configured_names(),
        "lsp_servers": config.lsp.keys().collect::<Vec<_>>(),
        "sandbox": {
            "mode": config.sandbox.mode,
            "allow_network": config.sandbox.allow_network,
        },
        "auth_required": state.token.is_some(),
        "cors": state.cors,
    }))
}

async fn config_info(State(state): State<AppState>) -> Json<Value> {
    let services = &state.services;
    let config = &services.config;
    let providers: Vec<Value> = config
        .providers
        .iter()
        .map(|(name, provider)| {
            json!({
                "name": name,
                "protocol": provider.protocol,
                "base_url": provider.base_url,
                "default_model": provider.default_model,
                "models": crate::providers::known_models_for(config, name),
                "enabled": provider.enabled,
                "requires_key": provider.requires_key,
                "ready": services.provider_runtime.has_key(name),
            })
        })
        .collect();
    Json(json!({
        "files": services.config_files,
        "warnings": services.warnings,
        "default_provider": config.default_provider,
        "model": config.model,
        "fallback_models": config.fallback_models,
        "max_tokens": config.max_tokens,
        "max_turns": config.max_turns,
        "effort": config.effort,
        "default_agent": config.default_agent,
        "auto_memory": config.auto_memory,
        "permission_mode": config.permission_mode,
        "sandbox": {
            "mode": config.sandbox.mode,
            "allow_network": config.sandbox.allow_network,
            "extra_writable": config.sandbox.extra_writable,
            "extra_readonly": config.sandbox.extra_readonly,
        },
        "permissions": {
            "allow": config.permissions.allow.len(),
            "ask": config.permissions.ask.len(),
            "deny": config.permissions.deny.len(),
            "additional_directories": config.permissions.additional_directories,
        },
        "context": {
            "window": config.context.window,
            "auto_compact": config.context.auto_compact,
            "compact_threshold": config.context.compact_threshold,
            "max_tool_output_chars": config.context.max_tool_output_chars,
            "max_tool_output_lines": config.context.max_tool_output_lines,
        },
        "checkpoints": {
            "enabled": config.checkpoints.enabled,
            "keep_turns": config.checkpoints.keep_turns,
        },
        "sessions": {
            "auto_save": config.sessions.auto_save,
            "retention_days": config.sessions.retention_days,
        },
        "providers": providers,
        "mcp_servers": services.mcp.configured_names(),
        "lsp_servers": config.lsp.keys().collect::<Vec<_>>(),
    }))
}

async fn models(State(state): State<AppState>) -> Json<Value> {
    let services = &state.services;
    let config = &services.config;
    let providers: Vec<Value> = all_known_models(config)
        .into_iter()
        .map(|(name, models)| {
            let provider = config.provider(&name);
            json!({
                "name": name,
                "protocol": provider.map(|value| value.protocol),
                "base_url": provider.map(|value| value.base_url.clone()),
                "default_model": provider.and_then(|value| value.default_model.clone()),
                "models": models,
                "ready": services.provider_runtime.has_key(&name),
                "enabled": provider.is_none_or(|value| value.enabled),
            })
        })
        .collect();
    Json(json!({
        "default_provider": config.default_provider,
        "model": config.model,
        "fallback_models": config.fallback_models,
        "providers": providers,
    }))
}

async fn list_agents(State(state): State<AppState>) -> Json<Value> {
    let agents = state.services.agents.all();
    let count = agents.len();
    Json(json!({
        "agents": agents,
        "count": count,
    }))
}

async fn get_agent(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let agent = state
        .services
        .agents
        .get(&name)
        .ok_or_else(|| ApiError::not_found(format!("unknown agent `{name}`")))?;
    Ok(Json(json!(agent)))
}

async fn list_skills(State(state): State<AppState>) -> Json<Value> {
    let skills = state.services.skills.all();
    let count = skills.len();
    Json(json!({
        "skills": skills,
        "count": count,
    }))
}

async fn get_skill(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let name = name.trim_start_matches('/');
    let skill = state
        .services
        .skills
        .get(name)
        .ok_or_else(|| ApiError::not_found(format!("unknown skill `{name}`")))?;
    Ok(Json(json!(skill)))
}

async fn list_sessions(
    State(state): State<AppState>,
    Query(query): Query<ListSessionsQuery>,
) -> Result<Json<Value>, ApiError> {
    let mut sessions = state.services.store.list()?;
    if let Some(cwd) = &query.cwd {
        let wanted = cwd.canonicalize().unwrap_or_else(|_| cwd.clone());
        sessions.retain(|session| {
            session
                .cwd
                .canonicalize()
                .unwrap_or_else(|_| session.cwd.clone())
                == wanted
        });
    }
    let limit = query
        .limit
        .unwrap_or(DEFAULT_SESSION_LIMIT)
        .min(MAX_SESSION_LIMIT);
    sessions.truncate(limit);
    let count = sessions.len();
    Ok(Json(json!({
        "sessions": sessions,
        "count": count,
    })))
}

async fn create_session(
    State(state): State<AppState>,
    Json(request): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<Session>), ApiError> {
    let mut session = state
        .services
        .new_session(
            request.provider.as_deref(),
            request.model.as_deref(),
            request.agent.as_deref(),
            request.permission_mode,
        )
        .map_err(|error| ApiError::bad_request(format!("{error:#}")))?;
    if let Some(name) = &request.name {
        session
            .set_name(Some(name.clone()))
            .map_err(|error| ApiError::bad_request(format!("{error:#}")))?;
    }
    state.services.save(&session)?;
    Ok((StatusCode::CREATED, Json(session)))
}

async fn resolve_session(state: &AppState, reference: &str) -> Result<Session, ApiError> {
    state
        .services
        .store
        .resolve(reference)
        .map_err(|error| ApiError::not_found(format!("{error:#}")))
}

async fn get_session(
    State(state): State<AppState>,
    AxumPath(reference): AxumPath<String>,
) -> Result<Json<Session>, ApiError> {
    Ok(Json(resolve_session(&state, &reference).await?))
}

async fn update_session(
    State(state): State<AppState>,
    AxumPath(reference): AxumPath<String>,
    Json(request): Json<UpdateSessionRequest>,
) -> Result<Json<Session>, ApiError> {
    let resolved = resolve_session(&state, &reference).await?;
    let _active = state.activate(&resolved.id, CancellationToken::new())?;
    let mut session = state.services.store.load(&resolved.id)?;
    state
        .services
        .apply_overrides(
            &mut session,
            request.provider.as_deref(),
            request.model.as_deref(),
            request.agent.as_deref(),
            request.permission_mode,
        )
        .map_err(|error| ApiError::bad_request(format!("{error:#}")))?;
    if let Some(name) = &request.name {
        session
            .set_name(Some(name.clone()))
            .map_err(|error| ApiError::bad_request(format!("{error:#}")))?;
    }
    state.services.save(&session)?;
    Ok(Json(session))
}

async fn delete_session(
    State(state): State<AppState>,
    AxumPath(reference): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let session = resolve_session(&state, &reference).await?;
    let _active = state.activate(&session.id, CancellationToken::new())?;
    state.services.store.delete(&session.id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn fork_session(
    State(state): State<AppState>,
    AxumPath(reference): AxumPath<String>,
    Json(request): Json<ForkSessionRequest>,
) -> Result<(StatusCode, Json<Session>), ApiError> {
    let source = resolve_session(&state, &reference).await?;
    let _active = state.activate(&source.id, CancellationToken::new())?;
    let mut forked = source.fork();
    if let Some(name) = &request.name {
        forked
            .set_name(Some(name.clone()))
            .map_err(|error| ApiError::bad_request(format!("{error:#}")))?;
    }
    state.services.save(&forked)?;
    Ok((StatusCode::CREATED, Json(forked)))
}

async fn start_turn(
    State(state): State<AppState>,
    AxumPath(reference): AxumPath<String>,
    Json(request): Json<TurnRequest>,
) -> Result<Response, ApiError> {
    let session = resolve_session(&state, &reference).await?;
    let cancel = CancellationToken::new();
    let active = state.activate(&session.id, cancel.clone())?;

    let TurnRequest {
        prompt,
        images,
        stream,
        auto_approve,
        max_turns,
        provider,
        model,
        agent,
        permission_mode,
    } = request;
    if prompt.trim().is_empty() && images.is_empty() {
        return Err(ApiError::bad_request("prompt must not be empty"));
    }
    for image in &images {
        if !image.media_type.starts_with("image/") {
            return Err(ApiError::bad_request(format!(
                "unsupported image media type `{}`; expected image/*",
                image.media_type
            )));
        }
        if image.data.trim().is_empty() {
            return Err(ApiError::bad_request("image data must not be empty"));
        }
    }

    let mut session = session;
    let first_turn_in_process = lock(&state.seen_sessions).insert(session.id.clone());
    if !first_turn_in_process && !session.messages.is_empty() {
        session.mark_context_handoff();
    }
    state
        .services
        .apply_overrides(
            &mut session,
            provider.as_deref(),
            model.as_deref(),
            agent.as_deref(),
            permission_mode,
        )
        .map_err(|error| ApiError::bad_request(format!("{error:#}")))?;
    state.services.save(&session)?;
    let message = user_message(prompt, &images);
    let session_id = session.id.clone();
    let title = session.display_title();

    if !stream {
        let gateway = Arc::new(HeadlessGateway::new(HeadlessOptions {
            format: OutputFormat::Json,
            auto_approve,
            show_thinking: false,
            show_tool_output: false,
            quiet: true,
        }));
        let runtime = build_runtime(&state, session, gateway, &cancel)?;
        let result = agent::run_turn(runtime.clone(), message, max_turns).await;
        let warning = runtime
            .save_session()
            .await
            .err()
            .map(|error| format!("could not save session: {error:#}"));
        let snapshot = runtime.session_snapshot().await;
        let result = make_result(&snapshot, result, warning);
        drop(active);
        return Ok(Json(result).into_response());
    }

    let (sender, mut receiver) = mpsc::unbounded_channel();
    let gateway = Arc::new(ServerGateway {
        events: sender.clone(),
        pending_permissions: state.pending_permissions.clone(),
        pending_questions: state.pending_questions.clone(),
        issued_permissions: StdMutex::new(HashSet::new()),
        issued_questions: StdMutex::new(HashSet::new()),
        cancel: cancel.clone(),
        auto_approve,
    });
    let runtime = build_runtime(&state, session, gateway.clone(), &cancel)?;
    let runtime_for_task = runtime.clone();
    let gateway_for_task = gateway.clone();

    tokio::spawn(async move {
        let _active = active;
        gateway_for_task.emit(AgentEvent::SessionStarted {
            id: session_id,
            title,
        });
        let result = agent::run_turn(runtime_for_task.clone(), message, max_turns).await;
        let warning = runtime_for_task
            .save_session()
            .await
            .err()
            .map(|error| format!("could not save session: {error:#}"));
        let snapshot = runtime_for_task.session_snapshot().await;
        let result = make_result(&snapshot, result, warning);
        let _ = sender.send(ServerEvent::Done(result));
    });

    let stream = async_stream::stream! {
        let _cancel = CancelOnDrop(cancel);
        let mut received_result = false;
        while let Some(event) = receiver.recv().await {
            match event {
                ServerEvent::Agent(event) => {
                    let name = agent_event_name(&event);
                    match serde_json::to_string(&event) {
                        Ok(data) => {
                            yield Ok::<Event, Infallible>(Event::default().event(name).data(data));
                        }
                        Err(error) => {
                            let data = json!({
                                "type": "error",
                                "text": format!("could not serialize event: {error}"),
                            });
                            yield Ok(Event::default().event("error").data(data.to_string()));
                        }
                    }
                }
                ServerEvent::Done(result) => {
                    received_result = true;
                    match serde_json::to_string(&result) {
                        Ok(data) => {
                            yield Ok(Event::default().event("result").data(data));
                        }
                        Err(error) => {
                            let data = json!({
                                "type": "error",
                                "text": format!("could not serialize result: {error}"),
                            });
                            yield Ok(Event::default().event("error").data(data.to_string()));
                        }
                    }
                    break;
                }
            }
        }
        if !received_result {
            let data = json!({
                "type": "error",
                "text": "the agent task ended without producing a result",
            });
            yield Ok(Event::default().event("error").data(data.to_string()));
        }
    };

    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response())
}

fn build_runtime(
    state: &AppState,
    session: Session,
    gateway: Arc<dyn UiGateway>,
    cancel: &CancellationToken,
) -> Result<Arc<crate::runtime::Runtime>, ApiError> {
    let mut runtime = state.services.runtime(session, gateway);
    let Some(inner) = Arc::get_mut(&mut runtime) else {
        return Err(ApiError::internal(
            "could not wire the turn cancellation token",
        ));
    };
    inner.cancel = cancel.clone();
    Ok(runtime)
}

fn make_result(
    session: &Session,
    result: Result<RunOutcome>,
    warning: Option<String>,
) -> TurnResult {
    let (text, stop_reason, usage, cost_usd, is_error, error, cancelled) = match result {
        Ok(outcome) => {
            let cancelled = outcome.stop_reason == "cancelled";
            (
                outcome.text,
                outcome.stop_reason,
                outcome.usage,
                outcome.cost_usd,
                false,
                None,
                cancelled,
            )
        }
        Err(error) => {
            let last_turn = session.turns.last();
            let stop_reason = session
                .last_stop_reason
                .clone()
                .unwrap_or_else(|| "error".into());
            let cancelled = stop_reason == "cancelled";
            (
                session.latest_assistant_text().unwrap_or_default(),
                stop_reason,
                last_turn.map_or_else(Usage::default, |turn| turn.usage),
                last_turn.map_or(0.0, |turn| turn.cost_usd),
                true,
                Some(format!("{error:#}")),
                cancelled,
            )
        }
    };
    TurnResult {
        kind: "result",
        session_id: session.id.clone(),
        turn: session.current_turn,
        text,
        stop_reason,
        usage,
        cost_usd,
        session_usage: session.usage,
        session_cost_usd: session.cost_usd,
        is_error,
        cancelled,
        error,
        warning,
    }
}

fn agent_event_name(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::SessionStarted { .. } => "session_started",
        AgentEvent::TurnStarted { .. } => "turn_started",
        AgentEvent::TextDelta(_) => "text_delta",
        AgentEvent::ThinkingDelta(_) => "thinking_delta",
        AgentEvent::ToolStarted { .. } => "tool_started",
        AgentEvent::ToolFinished { .. } => "tool_finished",
        AgentEvent::Notice(_) => "notice",
        AgentEvent::Warning(_) => "warning",
        AgentEvent::Error(_) => "error",
        AgentEvent::TodoUpdated(_) => "todo_updated",
        AgentEvent::Usage { .. } => "usage",
        AgentEvent::CompactionStarted => "compaction_started",
        AgentEvent::CompactionFinished { .. } => "compaction_finished",
        AgentEvent::SubagentStarted { .. } => "subagent_started",
        AgentEvent::SubagentFinished { .. } => "subagent_finished",
        AgentEvent::PermissionModeChanged(_) => "permission_mode_changed",
        AgentEvent::ModelChanged { .. } => "model_changed",
        AgentEvent::TurnFinished { .. } => "turn_finished",
        AgentEvent::RunFailed(_) => "run_failed",
        AgentEvent::QueuedUserMessage(_) => "queued_user_message",
        AgentEvent::PermissionRequested { .. } => "permission_requested",
        AgentEvent::PermissionResolved { .. } => "permission_resolved",
        AgentEvent::QuestionRequested { .. } => "question_requested",
        AgentEvent::QuestionResolved { .. } => "question_resolved",
    }
}

async fn answer_permission(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(decision): Json<PermissionDecision>,
) -> Result<StatusCode, ApiError> {
    let sender = lock(&state.pending_permissions)
        .remove(&id)
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "no pending permission request `{id}`; it may have expired, been cancelled, or already been answered"
            ))
        })?;
    sender.send(decision).map_err(|_| {
        ApiError::conflict("the agent is no longer waiting for this permission request")
    })?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct QuestionAnswer {
    #[serde(default)]
    answer: Option<UserAnswer>,
}

async fn answer_question(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<QuestionAnswer>,
) -> Result<StatusCode, ApiError> {
    let sender = lock(&state.pending_questions)
        .remove(&id)
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "no pending question `{id}`; it may have expired, been cancelled, or already been answered"
            ))
        })?;
    sender
        .send(request.answer)
        .map_err(|_| ApiError::conflict("the agent is no longer waiting for this question"))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn cancel_turn(
    State(state): State<AppState>,
    AxumPath(reference): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let session = resolve_session(&state, &reference).await?;
    let Some(cancel) = state.cancel_token(&session.id) else {
        return Err(ApiError::conflict(format!(
            "session `{}` has no active turn",
            session.id
        )));
    };
    cancel.cancel();
    Ok(Json(json!({
        "cancelled": true,
        "session_id": session.id,
    })))
}

async fn undo_turn(
    State(state): State<AppState>,
    AxumPath(reference): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let resolved = resolve_session(&state, &reference).await?;
    let _active = state.activate(&resolved.id, CancellationToken::new())?;
    let mut session = state.services.store.load(&resolved.id)?;
    let checkpointer = Checkpointer::new(
        state.services.paths.checkpoints.clone(),
        state.services.config.checkpoints.enabled,
    );
    let (turn, report) = checkpointer.undo_session_turn(&mut session)?;
    let Some(turn) = turn else {
        return Err(ApiError::conflict("there is no agent turn to undo"));
    };
    if !report.errors.is_empty() {
        return Err(ApiError::conflict(format!(
            "undo was only partially applied; messages were kept because file restoration failed:\n{}",
            report.render()
        )));
    }
    state.services.save(&session)?;
    let message = report.render();
    Ok(Json(json!({
        "undone_turn": turn,
        "message": message,
        "restored": report.restored,
        "removed": report.removed,
    })))
}

async fn mcp_info(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "servers": state.services.mcp.configured_names(),
        "status": state.services.mcp.render_status().await,
    }))
}

async fn mcp_discover(
    State(state): State<AppState>,
    Json(request): Json<McpDiscoverRequest>,
) -> Json<Value> {
    let names = request.server.map(|server| vec![server]);
    let discovery = state.services.mcp.discover(names.as_deref()).await;
    Json(json!({
        "tools": discovery.tools,
        "resources": discovery.resources,
        "prompts": discovery.prompts,
        "warnings": discovery.warnings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_comparison_is_exact() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secrets"));
        assert!(!constant_time_eq(b"secret", b"Secret"));
    }

    #[test]
    fn agent_events_serialize_to_stable_json() {
        let event = AgentEvent::TextDelta("hello".into());
        let value = serde_json::to_value(&event).expect("serialize text delta");
        assert_eq!(value["type"], "text_delta");
        assert_eq!(value["text"], "hello");
        assert_eq!(agent_event_name(&event), "text_delta");
    }

    #[test]
    fn permission_decisions_are_accepted_as_flat_json() {
        let decision: PermissionDecision =
            serde_json::from_str(r#"{"decision":"allow_for_session","rule":"Bash(git status)"}"#)
                .expect("parse permission decision");
        assert!(decision.is_allowed());
        let deny: PermissionDecision =
            serde_json::from_str(r#"{"decision":"deny","reason":"nope"}"#)
                .expect("parse deny decision");
        assert!(!deny.is_allowed());
    }
}
