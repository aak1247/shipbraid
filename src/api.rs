use std::{collections::BTreeSet, sync::Arc};

use axum::{
    Json, Router,
    body::Body,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, Request, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    config::ResolvedToken,
    domain::{
        CoverageMode, DeliveryRun, Event, LOCAL_TENANT_ID, Lane, Phase, Priority, ReleaseRequest,
        RequestContext, RequestStatus, State as DomainState,
    },
    service::{
        RequestActor as Actor, RequestMutation, Service, ServiceError, request_allowed_actions,
    },
};

const MAX_REQUEST_BODY: usize = 16 * 1024;
const MAX_REASON_BYTES: usize = 2 * 1024;
const MAX_LIST_LIMIT: usize = 200;

#[derive(Clone)]
struct ApiState {
    service: Arc<Service>,
    tokens: Vec<ResolvedToken>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: String,
    message: String,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    body: ErrorBody,
    retry_after: Option<u64>,
}

impl ApiError {
    fn new(status: StatusCode, code: &str, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            status,
            body: ErrorBody {
                code: code.into(),
                message: message.into(),
                retryable,
                request_id: None,
            },
            retry_after: None,
        }
    }

    fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "A valid bearer token is required.",
            false,
        )
    }

    fn forbidden() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "The configured token is not authorized for this operation.",
            false,
        )
    }

    fn bad_request(code: &str, message: &str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message, false)
    }

    fn missing(message: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message, false)
    }

    fn retryable(code: &str, message: &str, retry_after: u64) -> Self {
        let mut error = Self::new(StatusCode::TOO_MANY_REQUESTS, code, message, true);
        error.retry_after = Some(retry_after);
        error
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(self.body)).into_response();
        if let Some(seconds) = self.retry_after
            && let Ok(value) = seconds.to_string().parse()
        {
            response.headers_mut().insert("retry-after", value);
        }
        response
    }
}

impl From<ServiceError> for ApiError {
    fn from(error: ServiceError) -> Self {
        use crate::domain::DomainError;
        match error {
            ServiceError::Forbidden => Self::forbidden(),
            ServiceError::InvalidRequestContext(message) => Self::new(
                StatusCode::BAD_REQUEST,
                "invalid_request_context",
                message,
                false,
            ),
            ServiceError::Domain(DomainError::IdempotencyConflict) => Self::new(
                StatusCode::CONFLICT,
                "idempotency_conflict",
                "The Idempotency-Key is already bound to a different request body.",
                false,
            ),
            ServiceError::Domain(DomainError::LaneNotFound(_)) => Self::missing("Lane not found."),
            ServiceError::Domain(DomainError::RequestNotFound) => {
                Self::missing("Request not found.")
            }
            ServiceError::Domain(DomainError::RunNotFound) => Self::missing("Run not found."),
            ServiceError::Domain(DomainError::RequestNotOwned) => Self::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                "The request belongs to another principal.",
                false,
            ),
            ServiceError::Domain(DomainError::TargetNotAllowed(message)) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "target_not_allowed",
                message,
                false,
            ),
            ServiceError::Domain(DomainError::LineageUnproven) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "lineage_unproven",
                "The configured local Git history cannot prove this target is authorized.",
                false,
            ),
            ServiceError::Domain(DomainError::LaneBlocked(message)) => {
                Self::new(StatusCode::CONFLICT, "lane_blocked", message, true)
            }
            ServiceError::Domain(DomainError::DeadlineElapsed) => Self::new(
                StatusCode::CONFLICT,
                "deadline_elapsed",
                "The request deadline has elapsed.",
                false,
            ),
            ServiceError::Domain(DomainError::NotRetryable)
            | ServiceError::Domain(DomainError::RetryLimitExceeded) => Self::new(
                StatusCode::CONFLICT,
                "not_retryable",
                error.to_string(),
                false,
            ),
            ServiceError::Domain(DomainError::RequestTerminal) => Self::new(
                StatusCode::CONFLICT,
                "request_terminal",
                "A terminal request cannot be changed.",
                false,
            ),
            ServiceError::Domain(DomainError::InvalidTransition)
            | ServiceError::Domain(DomainError::StaleGeneration)
            | ServiceError::Domain(DomainError::ExecutorMismatch)
            | ServiceError::Domain(DomainError::OwnershipUnproven) => Self::new(
                StatusCode::CONFLICT,
                "invalid_transition",
                error.to_string(),
                false,
            ),
            ServiceError::Domain(other) => Self::new(
                StatusCode::CONFLICT,
                "operation_conflict",
                other.to_string(),
                false,
            ),
            ServiceError::QueueFull => Self::retryable(
                "queue_limit",
                "The lane has reached its pending request limit.",
                1,
            ),
            ServiceError::NotReady => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "not_ready",
                "The coordinator is not ready.",
                true,
            ),
            ServiceError::UnprovenExecutorState => Self::new(
                StatusCode::CONFLICT,
                "ownership_unproven",
                "The scripted executor could not independently prove the requested state.",
                false,
            ),
            ServiceError::Storage(error) => {
                tracing::error!(error = %error, "coordinator storage operation failed");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "The coordinator could not persist this operation.",
                    true,
                )
            }
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SubmitBody {
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    parameters: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    lane_id: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    environment: Option<String>,
    revision: String,
    components: Vec<String>,
    #[serde(default)]
    coverage_mode: CoverageMode,
    #[serde(default = "normal_priority")]
    priority: Priority,
    #[serde(default)]
    deadline_at: Option<DateTime<Utc>>,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default, deserialize_with = "nullable_collaborators")]
    collaborators: BTreeSet<String>,
}

fn nullable_collaborators<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeSet<String>, D::Error> {
    Ok(Option::<BTreeSet<String>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PriorityBody {
    priority: Priority,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PhaseGrantBody {
    generation: u64,
    next_phase: Phase,
    executor_ref: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutorEventKind {
    Started,
    Stopped,
    Failed,
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutorEventBody {
    run_id: Uuid,
    generation: u64,
    event: ExecutorEventKind,
    #[serde(default)]
    executor_ref: Option<String>,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    environment_known_unchanged: bool,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResolveAction {
    ConfirmStopped,
    AdoptRunning,
    RecordVerified,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveUnknownBody {
    action: ResolveAction,
    evidence: String,
    reason: String,
}

#[derive(Debug, Deserialize, Default)]
struct ListQuery {
    lane_id: Option<String>,
    project: Option<String>,
    environment: Option<String>,
    after: Option<u64>,
    limit: Option<usize>,
    task_id: Option<String>,
    principal: Option<String>,
    status: Option<RequestStatus>,
    cursor: Option<String>,
}

#[derive(Debug, Serialize)]
struct Items<T> {
    items: Vec<T>,
}

#[derive(Debug, Serialize)]
struct LogPage {
    items: Vec<LogItem>,
    next_cursor: u64,
}

#[derive(Debug, Serialize)]
struct LogItem {
    sequence: u64,
    timestamp: DateTime<Utc>,
    level: &'static str,
    message: String,
}

#[derive(Debug, Serialize)]
struct SystemView {
    api_version: &'static str,
    tenant_id: &'static str,
    principal: String,
    capabilities: Value,
    executor: String,
    draining: bool,
    version: &'static str,
}

pub fn router(service: Arc<Service>) -> Router {
    let dashboard_dir = service.dashboard_dir().map(std::path::Path::to_path_buf);
    let state = ApiState {
        tokens: service.tokens().to_vec(),
        service,
    };
    let protected = Router::new()
        .route("/system", get(system))
        .route("/lanes", get(list_lanes))
        .route("/lanes/{lane_id}", get(get_lane))
        .route("/lanes/{lane_id}/resolve-unknown", post(resolve_unknown))
        .route("/requests", get(list_requests).post(create_request))
        .route("/requests/{request_id}", get(get_request))
        .route("/requests/{request_id}/withdraw", post(withdraw_request))
        .route("/requests/{request_id}/retry", post(retry_request))
        .route("/requests/{request_id}/priority", post(set_priority))
        .route("/runs", get(list_runs))
        .route("/runs/{run_id}", get(get_run))
        .route("/runs/{run_id}/logs", get(run_logs))
        .route("/runs/{run_id}/phase-grants", post(phase_grant))
        .route("/events", get(list_events))
        .route("/executor-events", post(executor_event))
        .route("/drain", post(drain))
        .route("/resume", post(resume))
        .fallback(api_not_found)
        .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BODY))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state.clone());

    let mut router = Router::new()
        .route("/live", get(live))
        .route("/ready", get(ready))
        .nest("/v1", protected)
        .fallback(api_not_found)
        .with_state(state);
    if let Some(path) = dashboard_dir {
        router = router.fallback_service(
            tower_http::services::ServeDir::new(path).append_index_html_on_directories(true),
        );
    }
    router
}

async fn authenticate(
    State(state): State<ApiState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let Some(header) = request.headers().get(AUTHORIZATION) else {
        return ApiError::unauthorized().into_response();
    };
    let Ok(header) = header.to_str() else {
        return ApiError::unauthorized().into_response();
    };
    let Some(secret) = header.strip_prefix("Bearer ") else {
        return ApiError::unauthorized().into_response();
    };
    if secret.is_empty() || secret.len() > 8192 {
        return ApiError::unauthorized().into_response();
    }
    let presented: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
    let actor = state
        .tokens
        .iter()
        .find(|token| constant_time_eq(&presented, &token.token_hash))
        .map(|token| Actor {
            id: token.id.clone(),
            lanes: token.lanes.clone(),
            projects: token.projects.clone(),
            urgent: token.urgent,
            admin: token.admin,
            executor: token.executor,
        });
    let Some(actor) = actor else {
        return ApiError::unauthorized().into_response();
    };
    request.extensions_mut().insert(actor);
    next.run(request).await
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right.iter())
        .fold(0_u8, |diff, (left, right)| diff | (left ^ right))
        == 0
}

async fn live() -> Json<Value> {
    Json(json!({"status":"live"}))
}

async fn ready(State(state): State<ApiState>) -> Response {
    match state.service.ready().await {
        Ok(()) => (StatusCode::OK, Json(json!({"status":"ready"}))).into_response(),
        Err(error) => ApiError::from(error).into_response(),
    }
}

async fn system(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
) -> Result<Json<SystemView>, ApiError> {
    require_non_executor(&actor)?;
    let snapshot = state.service.snapshot().await;
    Ok(Json(SystemView {
        api_version: "1",
        tenant_id: LOCAL_TENANT_ID,
        principal: actor.id.clone(),
        capabilities: json!({"submit":true,"withdraw":true,"retry":true,"raise_priority":actor.urgent || actor.admin,"admin":actor.admin}),
        executor: state.service.executor_mix(),
        draining: snapshot.draining,
        version: env!("CARGO_PKG_VERSION"),
    }))
}

async fn list_lanes(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Items<Value>>, ApiError> {
    require_non_executor(&actor)?;
    let snapshot = state.service.snapshot().await;
    let lane_filter = scoped_lane_filter(
        &actor,
        query.lane_id.as_deref(),
        query.project.as_deref(),
        query.environment.as_deref(),
        &snapshot,
    )?;
    let items = snapshot
        .lanes
        .values()
        .filter(|lane| {
            lane_filter
                .as_ref()
                .is_none_or(|ids| ids.contains(&lane.id))
        })
        .map(|lane| lane_view(lane, state.service.executor_kind(&lane.id)))
        .collect();
    Ok(Json(Items { items }))
}

async fn get_lane(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(lane_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_non_executor(&actor)?;
    let snapshot = state.service.snapshot().await;
    let lane = snapshot
        .lanes
        .get(&lane_id)
        .ok_or_else(|| ApiError::missing("Lane not found."))?;
    require_lane(&actor, lane)?;
    Ok(Json(lane_view(lane, state.service.executor_kind(&lane.id))))
}

#[derive(Debug, Serialize)]
struct RequestPage {
    items: Vec<Value>,
    next_cursor: Option<String>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RequestCursor {
    created_at: DateTime<Utc>,
    lane_id: String,
    creation_sequence: u64,
    id: Uuid,
}

impl RequestCursor {
    fn from_request(request: &ReleaseRequest) -> Self {
        Self {
            created_at: request.created_at,
            lane_id: request.lane_id.clone(),
            creation_sequence: request.creation_sequence,
            id: request.id,
        }
    }
    fn encode(&self) -> String {
        // The append-only ledger retains the immutable ordering fields. A UUID
        // anchor keeps every generated cursor bounded regardless of lane length.
        format!("v1.{}", self.id.simple())
    }
    fn decode(
        value: &str,
        snapshot: &DomainState,
        lane_filter: Option<&BTreeSet<String>>,
    ) -> Result<Self, ApiError> {
        let invalid = || {
            ApiError::bad_request(
                "invalid_cursor",
                "cursor must be an unmodified request page cursor.",
            )
        };
        if value.len() != 35 {
            return Err(invalid());
        }
        let id = Uuid::parse_str(value.strip_prefix("v1.").ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
        let request = snapshot
            .requests
            .get(&id)
            .filter(|request| lane_filter.is_none_or(|lanes| lanes.contains(&request.lane_id)))
            .ok_or_else(invalid)?;
        Ok(Self::from_request(request))
    }
}

async fn list_requests(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<RequestPage>, ApiError> {
    require_non_executor(&actor)?;
    let Query(query) = query.map_err(|_| {
        ApiError::bad_request(
            "invalid_query",
            "Invalid request filters, status, or limit.",
        )
    })?;
    let limit = query.limit.unwrap_or(100);
    if !(1..=MAX_LIST_LIMIT).contains(&limit) {
        return Err(ApiError::bad_request(
            "invalid_limit",
            "limit must be between 1 and 200.",
        ));
    }
    let snapshot = state.service.snapshot().await;
    let lane_filter = scoped_lane_filter(
        &actor,
        query.lane_id.as_deref(),
        query.project.as_deref(),
        query.environment.as_deref(),
        &snapshot,
    )?;
    let cursor = query
        .cursor
        .as_deref()
        .map(|value| RequestCursor::decode(value, &snapshot, lane_filter.as_ref()))
        .transpose()?;
    let mut requests: Vec<_> = snapshot
        .requests
        .values()
        .filter(|request| {
            lane_filter
                .as_ref()
                .is_none_or(|lanes| lanes.contains(&request.lane_id))
                && query
                    .task_id
                    .as_ref()
                    .is_none_or(|task| request.context.task_id.as_ref() == Some(task))
                && query
                    .principal
                    .as_ref()
                    .is_none_or(|principal| request.principal == *principal)
                && query
                    .status
                    .as_ref()
                    .is_none_or(|status| request.status == *status)
                && cursor
                    .as_ref()
                    .is_none_or(|cursor| RequestCursor::from_request(request) < *cursor)
        })
        .collect();
    requests.sort_by_key(|request| std::cmp::Reverse(RequestCursor::from_request(request)));
    let has_more = requests.len() > limit;
    requests.truncate(limit);
    let next_cursor = if has_more {
        requests
            .last()
            .map(|request| RequestCursor::from_request(request).encode())
    } else {
        None
    };
    let items = requests
        .into_iter()
        .map(|request| request_view(request, &snapshot, &actor))
        .collect();
    Ok(Json(RequestPage { items, next_cursor }))
}

async fn get_request(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(request_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_non_executor(&actor)?;
    let id = parse_uuid(&request_id)?;
    let snapshot = state.service.snapshot().await;
    let request = snapshot
        .requests
        .get(&id)
        .ok_or_else(|| ApiError::missing("Request not found."))?;
    require_lane_id(&actor, &request.lane_id, &snapshot)?;
    Ok(Json(request_view(request, &snapshot, &actor)))
}

async fn list_runs(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Items<Value>>, ApiError> {
    require_non_executor(&actor)?;
    let snapshot = state.service.snapshot().await;
    let lane_filter = scoped_lane_filter(
        &actor,
        query.lane_id.as_deref(),
        query.project.as_deref(),
        query.environment.as_deref(),
        &snapshot,
    )?;
    let mut runs: Vec<_> = snapshot
        .runs
        .values()
        .filter(|run| {
            lane_filter
                .as_ref()
                .is_none_or(|lanes| lanes.contains(&run.lane_id))
        })
        .collect();
    runs.sort_by_key(|run| std::cmp::Reverse(run.created_at));
    let items = runs
        .into_iter()
        .map(|run| run_view(run, &snapshot))
        .collect();
    Ok(Json(Items { items }))
}

async fn get_run(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(run_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_non_executor(&actor)?;
    let id = parse_uuid(&run_id)?;
    let snapshot = state.service.snapshot().await;
    let run = snapshot
        .runs
        .get(&id)
        .ok_or_else(|| ApiError::missing("Run not found."))?;
    require_lane_id(&actor, &run.lane_id, &snapshot)?;
    Ok(Json(run_view(run, &snapshot)))
}

async fn list_events(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Items<Value>>, ApiError> {
    require_non_executor(&actor)?;
    let snapshot = state.service.snapshot().await;
    let lane_id = query.lane_id.as_deref().ok_or_else(|| {
        ApiError::bad_request(
            "lane_id_required",
            "lane_id is required for per-lane event cursors.",
        )
    })?;
    require_lane_id(&actor, lane_id, &snapshot)?;
    let after = query.after.unwrap_or_default();
    let limit = bounded_limit(query.limit);
    let items: Vec<_> = snapshot
        .events
        .iter()
        .filter(|event| event.lane_id == lane_id && event.sequence > after)
        .take(limit)
        .map(event_view)
        .collect();
    Ok(Json(Items { items }))
}

async fn run_logs(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(run_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<LogPage>, ApiError> {
    require_non_executor(&actor)?;
    let id = parse_uuid(&run_id)?;
    let snapshot = state.service.snapshot().await;
    let run = snapshot
        .runs
        .get(&id)
        .ok_or_else(|| ApiError::missing("Run not found."))?;
    require_lane_id(&actor, &run.lane_id, &snapshot)?;
    let after = query.after.unwrap_or_default();
    let limit = bounded_limit(query.limit);
    let selected: Vec<&Event> = snapshot
        .events
        .iter()
        .filter(|event| {
            event.run_id == Some(id) && event.sequence > after && event.kind == "executor_log"
        })
        .take(limit)
        .collect();
    let next_cursor = selected.last().map_or(after, |event| event.sequence);
    let items = selected
        .into_iter()
        .map(|event| {
            let (level, message) = event
                .message
                .strip_prefix('[')
                .and_then(|message| message.split_once("] "))
                .unwrap_or(("info", event.message.as_str()));
            LogItem {
                sequence: event.sequence,
                timestamp: event.timestamp,
                level: match level {
                    "error" | "warn" | "debug" => match level {
                        "error" => "error",
                        "warn" => "warn",
                        _ => "debug",
                    },
                    _ => "info",
                },
                message: sanitize_log(message),
            }
        })
        .collect();
    Ok(Json(LogPage { items, next_cursor }))
}

async fn create_request(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    headers: HeaderMap,
    body: Result<Json<SubmitBody>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    require_non_executor(&actor)?;
    let body = parse_body(body)?;
    validate_submit(&body)?;
    let snapshot = state.service.snapshot().await;
    let lane = resolve_lane_selector(
        &actor,
        body.lane_id.as_deref(),
        body.project.as_deref(),
        body.environment.as_deref(),
        &snapshot,
    )?
    .clone();
    if body.priority == Priority::Urgent && !actor.admin && !actor.urgent {
        return Err(ApiError::forbidden());
    }
    if body.coverage_mode == CoverageMode::Descendant && !lane.descendant_coverage_enabled {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "target_not_allowed",
            "Descendant coverage is disabled for this lane.",
            false,
        ));
    }
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or_else(|| {
            ApiError::bad_request(
                "invalid_idempotency_key",
                "A nonempty Idempotency-Key of at most 128 bytes is required.",
            )
        })?;
    let mut components = body.components.clone();
    components.sort();
    let mut body_value = json!({
        "lane_id": &lane.id,
        "revision": &body.revision,
        "components": &components,
        "coverage_mode": &body.coverage_mode,
        "priority": &body.priority,
        "deadline_at": &body.deadline_at,
        "reason": &body.reason,
    });
    // Preserve the exact old digest for empty context and session-only submissions.
    if body.task_id.is_some() || !body.collaborators.is_empty() {
        body_value["context"] = json!({"task_id":body.task_id,"collaborators":body.collaborators});
    }
    if !body.parameters.is_empty() {
        body_value["parameters"] = json!(body.parameters);
    }
    let request_hash = hex::encode(Sha256::digest(
        serde_json::to_vec(&body_value).unwrap_or_default(),
    ));
    let outcome = state
        .service
        .submit_with_parameters(
            actor.id.clone(),
            idempotency_key.to_owned(),
            request_hash,
            lane.id,
            body.revision,
            body.components,
            body.coverage_mode,
            body.priority,
            body.deadline_at,
            body.reason,
            RequestContext {
                task_id: body.task_id,
                session_id: body.session_id,
                collaborators: body.collaborators,
            },
            body.parameters,
        )
        .await?;
    let snapshot = state.service.snapshot().await;
    let request = snapshot.requests.get(&outcome.request_id).ok_or_else(|| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "The persisted request could not be read.",
            true,
        )
    })?;
    let mut view = request_view(request, &snapshot, &actor);
    if let Some(object) = view.as_object_mut() {
        object.insert("reused".into(), Value::Bool(outcome.reused));
        object.insert(
            "attached_run_id".into(),
            outcome
                .attached_run_id
                .map(|id| json!(id))
                .unwrap_or(Value::Null),
        );
        if let Some(lane) = snapshot.lanes.get(&request.lane_id) {
            object.insert("blocked_reason".into(), json!(lane.blocked_reason));
        }
    }
    Ok((StatusCode::ACCEPTED, Json(view)))
}

async fn withdraw_request(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(request_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_non_executor(&actor)?;
    let id = parse_uuid(&request_id)?;
    let before = state.service.snapshot().await;
    let request = before
        .requests
        .get(&id)
        .ok_or_else(|| ApiError::missing("Request not found."))?;
    require_lane_id(&actor, &request.lane_id, &before)?;
    state
        .service
        .mutate_request(id, &actor, RequestMutation::Withdraw)
        .await?;
    let snapshot = state.service.snapshot().await;
    let request = snapshot
        .requests
        .get(&id)
        .ok_or_else(|| ApiError::missing("Request not found."))?;
    Ok(Json(request_view(request, &snapshot, &actor)))
}

async fn retry_request(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(request_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_non_executor(&actor)?;
    let id = parse_uuid(&request_id)?;
    let before = state.service.snapshot().await;
    let request = before
        .requests
        .get(&id)
        .ok_or_else(|| ApiError::missing("Request not found."))?;
    require_lane_id(&actor, &request.lane_id, &before)?;
    state
        .service
        .mutate_request(id, &actor, RequestMutation::Retry)
        .await?;
    let snapshot = state.service.snapshot().await;
    let request = snapshot
        .requests
        .get(&id)
        .ok_or_else(|| ApiError::missing("Request not found."))?;
    Ok(Json(request_view(request, &snapshot, &actor)))
}

async fn set_priority(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(request_id): Path<String>,
    body: Result<Json<PriorityBody>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    require_non_executor(&actor)?;
    let body = parse_body(body)?;
    validate_reason(&body.reason)?;
    if body.priority == Priority::Urgent && !actor.admin && !actor.urgent {
        return Err(ApiError::forbidden());
    }
    let id = parse_uuid(&request_id)?;
    let before = state.service.snapshot().await;
    let request = before
        .requests
        .get(&id)
        .ok_or_else(|| ApiError::missing("Request not found."))?;
    require_lane_id(&actor, &request.lane_id, &before)?;
    let reason = if body.reason.is_empty() {
        format!("priority changed through API by {}", actor.id)
    } else {
        body.reason
    };
    state
        .service
        .mutate_request(id, &actor, RequestMutation::Priority(body.priority, reason))
        .await?;
    let snapshot = state.service.snapshot().await;
    let request = snapshot
        .requests
        .get(&id)
        .ok_or_else(|| ApiError::missing("Request not found."))?;
    Ok(Json(request_view(request, &snapshot, &actor)))
}

async fn phase_grant(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(run_id): Path<String>,
    body: Result<Json<PhaseGrantBody>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    require_executor(&actor)?;
    let body = parse_body(body)?;
    let id = parse_uuid(&run_id)?;
    let snapshot = state.service.snapshot().await;
    let run = snapshot
        .runs
        .get(&id)
        .ok_or_else(|| ApiError::missing("Run not found."))?;
    require_lane(
        &actor,
        snapshot
            .lanes
            .get(&run.lane_id)
            .ok_or_else(|| ApiError::missing("Lane not found."))?,
    )?;
    state
        .service
        .phase_grant(id, body.generation, body.next_phase, body.executor_ref)
        .await?;
    let snapshot = state.service.snapshot().await;
    let run = snapshot
        .runs
        .get(&id)
        .ok_or_else(|| ApiError::missing("Run not found."))?;
    Ok(Json(run_view(run, &snapshot)))
}

async fn executor_event(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    body: Result<Json<ExecutorEventBody>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    require_executor(&actor)?;
    let body = parse_body(body)?;
    if body.reason.len() > MAX_REASON_BYTES {
        return Err(ApiError::bad_request(
            "invalid_request",
            "reason exceeds 2 KiB.",
        ));
    }
    let snapshot = state.service.snapshot().await;
    let run = snapshot
        .runs
        .get(&body.run_id)
        .ok_or_else(|| ApiError::missing("Run not found."))?;
    let lane = snapshot
        .lanes
        .get(&run.lane_id)
        .ok_or_else(|| ApiError::missing("Lane not found."))?;
    require_lane(&actor, lane)?;
    state
        .service
        .executor_event(
            body.run_id,
            body.generation,
            body.event,
            body.executor_ref,
            body.reason,
            body.environment_known_unchanged,
        )
        .await?;
    let snapshot = state.service.snapshot().await;
    let run = snapshot
        .runs
        .get(&body.run_id)
        .ok_or_else(|| ApiError::missing("Run not found."))?;
    Ok(Json(run_view(run, &snapshot)))
}

async fn resolve_unknown(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(lane_id): Path<String>,
    body: Result<Json<ResolveUnknownBody>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&actor)?;
    let body = parse_body(body)?;
    validate_reason(&body.evidence)?;
    validate_reason(&body.reason)?;
    let snapshot = state.service.snapshot().await;
    let lane = snapshot
        .lanes
        .get(&lane_id)
        .ok_or_else(|| ApiError::missing("Lane not found."))?;
    require_lane(&actor, lane)?;
    state
        .service
        .resolve_unknown(lane_id.clone(), body.action, body.evidence, body.reason)
        .await?;
    let snapshot = state.service.snapshot().await;
    let lane = snapshot
        .lanes
        .get(&lane_id)
        .ok_or_else(|| ApiError::missing("Lane not found."))?;
    Ok(Json(lane_view(lane, state.service.executor_kind(&lane.id))))
}

async fn drain(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&actor)?;
    parse_body(body)?;
    state.service.set_draining(true).await?;
    Ok(Json(json!({"draining":true})))
}

async fn resume(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&actor)?;
    parse_body(body)?;
    state.service.set_draining(false).await?;
    Ok(Json(json!({"draining":false})))
}

async fn api_not_found() -> ApiError {
    ApiError::missing("API route not found.")
}

fn parse_body<T: DeserializeOwned>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    body.map(|Json(value)| value).map_err(|_| {
        ApiError::bad_request(
            "invalid_json",
            "Request body must be a valid JSON object within the configured size limit.",
        )
    })
}

fn validate_submit(body: &SubmitBody) -> Result<(), ApiError> {
    if body
        .lane_id
        .as_ref()
        .is_some_and(|lane_id| lane_id.is_empty() || lane_id.len() > 128)
        || body
            .project
            .as_ref()
            .is_some_and(|project| project.is_empty() || project.len() > 128)
        || body
            .environment
            .as_ref()
            .is_some_and(|environment| environment.is_empty() || environment.len() > 128)
    {
        return Err(ApiError::bad_request(
            "invalid_request",
            "lane selectors must be nonempty and at most 128 bytes.",
        ));
    }
    if body.lane_id.is_none() && (body.project.is_none() || body.environment.is_none()) {
        return Err(ApiError::bad_request(
            "lane_selector_required",
            "Supply lane_id or both project and environment.",
        ));
    }
    if body.project.is_some() != body.environment.is_some() {
        return Err(ApiError::bad_request(
            "invalid_lane_selector",
            "project and environment must be supplied together.",
        ));
    }
    if !crate::lineage::is_full_revision(&body.revision) {
        return Err(ApiError::bad_request(
            "invalid_request",
            "revision must be a full immutable Git object ID.",
        ));
    }
    if body.components.is_empty() || body.components.len() > 64 {
        return Err(ApiError::bad_request(
            "invalid_request",
            "components must contain between 1 and 64 names.",
        ));
    }
    let mut unique = BTreeSet::new();
    if body.components.iter().any(|component| {
        component.is_empty()
            || component.len() > 128
            || !component
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            || !unique.insert(component)
    }) {
        return Err(ApiError::bad_request(
            "invalid_request",
            "component names must be unique safe identifiers.",
        ));
    }
    validate_reason(&body.reason)
}

fn validate_reason(reason: &str) -> Result<(), ApiError> {
    if reason.len() > MAX_REASON_BYTES {
        Err(ApiError::bad_request(
            "invalid_request",
            "reason exceeds 2 KiB.",
        ))
    } else {
        Ok(())
    }
}

fn parse_uuid(value: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(value)
        .map_err(|_| ApiError::bad_request("invalid_id", "Resource ID must be a UUID."))
}

fn bounded_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(MAX_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT)
}

fn normal_priority() -> Priority {
    Priority::Normal
}

fn lane_allowed(actor: &Actor, lane: &Lane) -> bool {
    actor.lane_allowed(lane)
}

fn require_lane(actor: &Actor, lane: &Lane) -> Result<(), ApiError> {
    if lane_allowed(actor, lane) {
        Ok(())
    } else {
        Err(ApiError::forbidden())
    }
}

fn require_lane_id(actor: &Actor, lane_id: &str, snapshot: &DomainState) -> Result<(), ApiError> {
    let lane = snapshot
        .lanes
        .get(lane_id)
        .ok_or_else(|| ApiError::missing("Lane not found."))?;
    require_lane(actor, lane)
}

fn scoped_lane_filter(
    actor: &Actor,
    requested: Option<&str>,
    project: Option<&str>,
    environment: Option<&str>,
    snapshot: &DomainState,
) -> Result<Option<BTreeSet<String>>, ApiError> {
    if requested.is_some() || project.is_some() || environment.is_some() {
        let lane = resolve_lane_selector(actor, requested, project, environment, snapshot)?;
        return Ok(Some(BTreeSet::from([lane.id.clone()])));
    }
    if actor.admin {
        return Ok(None);
    }
    Ok(Some(
        snapshot
            .lanes
            .values()
            .filter(|lane| lane_allowed(actor, lane))
            .map(|lane| lane.id.clone())
            .collect(),
    ))
}

fn resolve_lane_selector<'a>(
    actor: &Actor,
    requested: Option<&str>,
    project: Option<&str>,
    environment: Option<&str>,
    snapshot: &'a DomainState,
) -> Result<&'a Lane, ApiError> {
    if project.is_some() != environment.is_some() {
        return Err(ApiError::bad_request(
            "invalid_lane_selector",
            "project and environment must be supplied together.",
        ));
    }
    let lane = if let Some(lane_id) = requested {
        let lane = snapshot
            .lanes
            .get(lane_id)
            .ok_or_else(|| ApiError::missing("Lane not found."))?;
        if project.is_some_and(|value| value != lane.project)
            || environment.is_some_and(|value| value != lane.environment)
        {
            return Err(ApiError::bad_request(
                "lane_selector_mismatch",
                "lane_id does not match the supplied project and environment.",
            ));
        }
        lane
    } else {
        let (Some(project), Some(environment)) = (project, environment) else {
            return Err(ApiError::bad_request(
                "lane_selector_required",
                "Supply lane_id or both project and environment.",
            ));
        };
        let matching = snapshot
            .lanes
            .values()
            .filter(|lane| lane.project == project && lane.environment == environment)
            .collect::<Vec<_>>();
        match matching.as_slice() {
            [] => return Err(ApiError::missing("Lane not found.")),
            [lane] => *lane,
            _ => {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "ambiguous_lane",
                    "The project and environment resolve to more than one configured lane.",
                    false,
                ));
            }
        }
    };
    require_lane(actor, lane)?;
    Ok(lane)
}

fn require_non_executor(actor: &Actor) -> Result<(), ApiError> {
    if actor.executor && !actor.admin {
        Err(ApiError::forbidden())
    } else {
        Ok(())
    }
}

fn require_executor(actor: &Actor) -> Result<(), ApiError> {
    if actor.executor {
        Ok(())
    } else {
        Err(ApiError::forbidden())
    }
}

fn require_admin(actor: &Actor) -> Result<(), ApiError> {
    if actor.admin {
        Ok(())
    } else {
        Err(ApiError::forbidden())
    }
}

fn lane_view(lane: &Lane, executor_kind: &str) -> Value {
    json!({
        "id": lane.id,
        "tenant_id": lane.tenant_id,
        "repository_id": lane.repository_id,
        "components": lane.allowed_components,
        "request_parameters": lane.request_parameters,
        "coverage_modes": if lane.descendant_coverage_enabled { vec!["exact", "descendant"] } else { vec!["exact"] },
        "project": lane.project,
        "environment": lane.environment,
        "executor_kind": executor_kind,
        "profile_digest": lane.profile_digest,
        "active_run_id": lane.active_run_id,
        "generation": lane.generation,
        "blocked_reason": lane.blocked_reason,
        "enabled": lane.enabled,
        "release_line_id": lane.release_line_id,
        "config_version": lane.config_version,
        "descendant_coverage_enabled": lane.descendant_coverage_enabled,
        "preemption_enabled": lane.preemption_enabled,
        "failure_paused": lane.failure_paused
    })
}

fn request_view(request: &ReleaseRequest, snapshot: &DomainState, actor: &Actor) -> Value {
    let receipt = request
        .satisfied_by_receipt
        .and_then(|id| snapshot.receipts.get(&id));
    json!({
        "id": request.id,
        "tenant_id": request.tenant_id,
        "task_id": request.context.task_id,
        "session_id": request.context.session_id,
        "collaborators": request.context.collaborators,
        "waiting_reason": request.waiting_reason,
        "blocked_reason": snapshot.lanes.get(&request.lane_id).and_then(|lane| lane.blocked_reason.as_ref()),
        "allowed_actions": request_allowed_actions(actor, request, snapshot, Utc::now()),
        "lane_id": request.lane_id,
        "revision": request.target.revision,
        "components": request.target.components,
        "parameters": request.target.parameters,
        "coverage_mode": request.target.coverage_mode,
        "priority": request.priority,
        "status": request.status,
        "current_run_id": request.current_run_id,
        "satisfied_by_run": request.satisfied_by_run,
        "created_at": request.created_at,
        "deadline_at": request.deadline_at,
        "reason": request.reason,
        "satisfied_by_receipt": request.satisfied_by_receipt,
        "principal": request.principal,
        "creation_sequence": request.creation_sequence,
        "auto_retries_used": request.auto_retries_used,
        "manual_retries_used": request.manual_retries_used,
        "receipt": receipt
    })
}

fn run_view(run: &DeliveryRun, snapshot: &DomainState) -> Value {
    let receipt = run.receipt_id.and_then(|id| snapshot.receipts.get(&id));
    json!({
        "id": run.id,
        "lane_id": run.lane_id,
        "revision": run.target.revision,
        "components": run.target.components,
        "parameters": run.target.parameters,
        "profile_digest": run.target.profile_digest,
        "coverage_mode": run.target.coverage_mode,
        "effective_priority": run.effective_priority,
        "state": run.state,
        "phase": run.phase,
        "generation": run.generation,
        "replacement_run_id": run.replacement_run_id,
        "created_at": run.created_at,
        "updated_at": run.updated_at,
        "executor_ref": run.executor_ref,
        "receipt_id": run.receipt_id,
        "receipt": receipt
    })
}

fn event_view(event: &Event) -> Value {
    json!({
        "sequence": event.sequence,
        "actor": event.actor,
        "lane_id": event.lane_id,
        "run_id": event.run_id,
        "request_id": event.request_id,
        "timestamp": event.timestamp,
        "kind": event.kind,
        "message": event.message
    })
}

fn sanitize_log(message: &str) -> String {
    let message = message
        .chars()
        .filter(|character| !character.is_control() || *character == '\t')
        .take(2048)
        .collect::<String>();
    if message.to_ascii_lowercase().contains("bearer ") || message.contains("token=") {
        "[redacted scripted log content]".into()
    } else {
        message
    }
}

#[allow(dead_code)]
fn _assert_public_entity_shapes(_: &Lane, _: &ReleaseRequest, _: &DeliveryRun) {}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        process::{Command, Stdio},
    };

    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::{
        config::{Config, LaneConfig, LaneExecutorConfig, TokenConfig},
        service::Service,
    };

    fn git(repo: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    async fn send(
        app: Router,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&str>,
    ) -> Response {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        app.oneshot(
            builder
                .body(body.map_or_else(Body::empty, |body| Body::from(body.to_owned())))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    struct TeamFixture {
        _temp: tempfile::TempDir,
        service: Arc<Service>,
        app: Router,
        tokens: std::collections::BTreeMap<String, String>,
        revision: String,
    }

    async fn team_fixture() -> TeamFixture {
        team_fixture_with_lane("lane-one").await
    }

    async fn team_fixture_with_lane(lane_one_id: &str) -> TeamFixture {
        team_fixture_with_parameters(lane_one_id, Default::default()).await
    }

    async fn team_fixture_with_parameters(
        lane_one_id: &str,
        parameters: std::collections::BTreeMap<String, crate::domain::ParameterSpec>,
    ) -> TeamFixture {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-b", "release"]);
        git(&work, &["config", "user.name", "Team Fixture"]);
        git(&work, &["config", "user.email", "fixture@example.invalid"]);
        std::fs::write(work.join("app.txt"), "isolated fixture").unwrap();
        git(&work, &["add", "app.txt"]);
        git(&work, &["commit", "-m", "fixture"]);
        let revision = git(&work, &["rev-parse", "HEAD"]);
        let mirror = temp.path().join("source.git");
        assert!(
            Command::new("git")
                .args(["clone", "--mirror"])
                .arg(&work)
                .arg(&mirror)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        let mut tokens = std::collections::BTreeMap::new();
        let mut env_names = Vec::new();
        let configured = ["alice", "bob", "carol", "admin", "outside", "executor"]
            .into_iter()
            .map(|id| {
                let secret = format!("isolated-{}-{}", id, Uuid::new_v4());
                let name = format!("SHIPBRAID_API_TEST_{}", Uuid::new_v4().simple());
                // Each test owns these unique environment names until Service resolves them.
                unsafe {
                    std::env::set_var(&name, &secret);
                }
                tokens.insert(id.to_owned(), secret);
                env_names.push(name.clone());
                TokenConfig {
                    id: id.into(),
                    token_env: name,
                    lanes: vec![
                        if id == "outside" {
                            "lane-two"
                        } else {
                            lane_one_id
                        }
                        .into(),
                    ],
                    projects: vec![],
                    urgent: id == "alice",
                    admin: id == "admin",
                    executor: id == "executor",
                }
            })
            .collect();
        let lane = |id: &str, project: &str| LaneConfig {
            id: id.into(),
            project: project.into(),
            environment: "test".into(),
            repository_id: "repo".into(),
            repository_path: mirror.clone(),
            release_ref: "refs/heads/release".into(),
            profile_digest: "profile".into(),
            components: vec!["app".into()],
            allow_descendant: true,
            allow_preemption: true,
            failure_observe: None,
            request_parameters: parameters.clone(),
            enabled: true,
            executor: LaneExecutorConfig::Scripted,
        };
        let service = Service::open(Config {
            database_path: temp.path().join("ledger.db"),
            tokens: configured,
            lanes: vec![lane(lane_one_id, "lane-one"), lane("lane-two", "lane-two")],
            ..Config::default()
        })
        .await
        .unwrap();
        for name in env_names {
            unsafe {
                std::env::remove_var(name);
            }
        }
        let app = router(service.clone());
        TeamFixture {
            _temp: temp,
            service,
            app,
            tokens,
            revision,
        }
    }

    impl TeamFixture {
        fn body(&self) -> Value {
            json!({"lane_id":"lane-one","revision":self.revision,"components":["app"],"coverage_mode":"exact","priority":"normal","reason":"fixture"})
        }
        async fn submit(&self, actor: &str, key: &str, body: Value) -> (StatusCode, Value) {
            let response = self
                .app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/requests")
                        .header(AUTHORIZATION, format!("Bearer {}", self.tokens[actor]))
                        .header("content-type", "application/json")
                        .header("idempotency-key", key)
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            response_json(response).await
        }
        async fn call(
            &self,
            actor: &str,
            method: &str,
            path: &str,
            body: Option<Value>,
        ) -> (StatusCode, Value) {
            let body = body.map(|value| value.to_string());
            response_json(
                send(
                    self.app.clone(),
                    method,
                    path,
                    Some(&self.tokens[actor]),
                    body.as_deref(),
                )
                .await,
            )
            .await
        }
    }

    async fn response_json(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                panic!("non-JSON response: {}", String::from_utf8_lossy(&bytes))
            }),
        )
    }

    #[tokio::test]
    async fn team_mutation_permissions_and_actor_audit_are_consistent() {
        let f = team_fixture().await;
        let mut body = f.body();
        body["task_id"] = json!("task-17");
        body["session_id"] = json!("creation-session");
        body["collaborators"] = json!(["bob"]);
        let (status, request) = f.submit("alice", "team", body).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(request["tenant_id"], "local");
        assert_eq!(request["task_id"], "task-17");
        let id = request["id"].as_str().unwrap();
        let prefix = f.service.snapshot().await.events;
        for action in ["withdraw", "retry", "priority"] {
            let body = (action == "priority").then(|| json!({"priority":"urgent"}));
            assert_eq!(
                f.call(
                    "carol",
                    "POST",
                    &format!("/v1/requests/{id}/{action}"),
                    body
                )
                .await
                .0,
                StatusCode::FORBIDDEN
            );
        }
        let (_, view) = f
            .call("carol", "GET", &format!("/v1/requests/{id}"), None)
            .await;
        assert_eq!(view["allowed_actions"], json!([]));
        let (_, view) = f
            .call("bob", "GET", &format!("/v1/requests/{id}"), None)
            .await;
        assert_eq!(view["allowed_actions"], json!(["withdraw"]));
        assert_eq!(
            f.call(
                "bob",
                "POST",
                &format!("/v1/requests/{id}/priority"),
                Some(json!({"priority":"urgent"}))
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            f.call(
                "admin",
                "POST",
                &format!("/v1/requests/{id}/priority"),
                Some(json!({"priority":"urgent"}))
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            f.call("bob", "POST", &format!("/v1/requests/{id}/withdraw"), None)
                .await
                .0,
            StatusCode::OK
        );
        let snapshot = f.service.snapshot().await;
        assert_eq!(&snapshot.events[..prefix.len()], &prefix);
        let actor_for = |kind: &str| {
            snapshot
                .events
                .iter()
                .find(|event| event.kind == kind)
                .unwrap()
                .actor
                .as_deref()
        };
        assert_eq!(actor_for("request_accepted"), Some("alice"));
        assert_eq!(actor_for("request_priority_raised"), Some("admin"));
        assert_eq!(actor_for("request_withdrawn"), Some("bob"));
        for collaborator in ["outside", "executor", "unknown"] {
            let mut body = f.body();
            body["collaborators"] = json!([collaborator]);
            assert_eq!(
                f.submit("alice", collaborator, body).await.0,
                StatusCode::BAD_REQUEST
            );
        }
        for field in ["tenant_id", "principal"] {
            let mut body = f.body();
            body[field] = json!("forged");
            assert_eq!(
                f.submit("alice", field, body).await.0,
                StatusCode::BAD_REQUEST
            );
        }
        let mut body = f.body();
        body["task_id"] = json!("   ");
        assert_eq!(
            f.submit("alice", "invalid-task", body).await.0,
            StatusCode::BAD_REQUEST
        );
        let (_, own) = f.submit("alice", "owner", f.body()).await;
        assert_eq!(
            f.call(
                "alice",
                "POST",
                &format!("/v1/requests/{}/withdraw", own["id"].as_str().unwrap()),
                None
            )
            .await
            .0,
            StatusCode::OK
        );
        let (_, own) = f.submit("alice", "admin", f.body()).await;
        assert_eq!(
            f.call(
                "admin",
                "POST",
                &format!("/v1/requests/{}/withdraw", own["id"].as_str().unwrap()),
                None
            )
            .await
            .0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn context_hash_preserves_legacy_and_creation_session() {
        let f = team_fixture().await;
        let old = f.body();
        let expected_hash = hex::encode(Sha256::digest(serde_json::to_vec(&json!({"lane_id":"lane-one","revision":f.revision,"components":["app"],"coverage_mode":"exact","priority":"normal","deadline_at":null,"reason":"fixture"})).unwrap()));
        let (_, first) = f.submit("alice", "old", old.clone()).await;
        let id = Uuid::parse_str(first["id"].as_str().unwrap()).unwrap();
        assert_eq!(
            f.service.snapshot().await.requests[&id].request_hash,
            expected_hash
        );
        let mut session_only = old.clone();
        session_only["session_id"] = json!("new-session");
        session_only["task_id"] = Value::Null;
        session_only["collaborators"] = Value::Null;
        let (status, replay) = f.submit("alice", "old", session_only).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(replay["id"], first["id"]);
        assert!(replay["session_id"].is_null());
        let mut context = old.clone();
        context["task_id"] = json!("task");
        context["session_id"] = json!("first-session");
        context["collaborators"] = json!(["bob", "carol", "bob"]);
        let (_, first) = f.submit("alice", "context", context.clone()).await;
        context["session_id"] = json!("other-session");
        context["collaborators"] = json!(["carol", "bob"]);
        let (status, replay) = f.submit("alice", "context", context.clone()).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(replay["id"], first["id"]);
        assert_eq!(replay["session_id"], "first-session");
        context["task_id"] = json!("different-task");
        assert_eq!(
            f.submit("alice", "context", context).await.0,
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn request_pagination_filters_and_discovery_support_agent_recovery() {
        let f = team_fixture().await;
        let mut original = Vec::new();
        for n in 0..5 {
            let mut body = f.body();
            body["task_id"] = json!("recover");
            let (_, view) = f.submit("alice", &format!("page-{n}"), body).await;
            original.push(view["id"].clone());
        }
        let (status, first) = f
            .call(
                "alice",
                "GET",
                "/v1/requests?limit=2&task_id=recover&principal=alice&status=waiting",
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["items"].as_array().unwrap().len(), 2);
        let mut body = f.body();
        body["task_id"] = json!("recover");
        let (_, inserted) = f.submit("alice", "newer", body).await;
        let mut gathered = first["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].clone())
            .collect::<Vec<_>>();
        let mut cursor = first["next_cursor"].as_str().map(str::to_owned);
        while let Some(anchor) = cursor {
            let (status, page) = f
                .call(
                    "alice",
                    "GET",
                    &format!("/v1/requests?limit=2&task_id=recover&cursor={anchor}"),
                    None,
                )
                .await;
            assert_eq!(status, StatusCode::OK);
            gathered.extend(
                page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item["id"].clone()),
            );
            cursor = page["next_cursor"].as_str().map(str::to_owned);
        }
        original.reverse();
        assert_eq!(gathered, original);
        assert!(!gathered.contains(&inserted["id"]));
        for query in [
            "limit=0",
            "limit=201",
            "limit=no",
            "status=no",
            "cursor=bad",
            "cursor=v1.7b7d",
        ] {
            assert_eq!(
                f.call("alice", "GET", &format!("/v1/requests?{query}"), None)
                    .await
                    .0,
                StatusCode::BAD_REQUEST
            );
        }
        let (_, none) = f
            .call("alice", "GET", "/v1/requests?principal=bob", None)
            .await;
        assert_eq!(none["items"], json!([]));
        let (_, system) = f.call("bob", "GET", "/v1/system", None).await;
        assert_eq!(system["api_version"], "1");
        assert_eq!(system["principal"], "bob");
        assert_eq!(system["capabilities"]["raise_priority"], false);
        let (_, lane) = f.call("bob", "GET", "/v1/lanes/lane-one", None).await;
        assert_eq!(lane["repository_id"], "repo");
        assert_eq!(lane["components"], json!(["app"]));
        assert!(lane.get("repository_path").is_none());
        let (_, view) = f
            .call(
                "alice",
                "GET",
                &format!("/v1/requests/{}", inserted["id"].as_str().unwrap()),
                None,
            )
            .await;
        for field in ["waiting_reason", "blocked_reason", "allowed_actions"] {
            assert!(view.get(field).is_some());
        }
    }

    #[tokio::test]
    async fn request_cursor_round_trips_long_lane_and_hides_inaccessible_anchors() {
        let long_lane = "l".repeat(3_000);
        let f = team_fixture_with_lane(&long_lane).await;
        let mut ids = Vec::new();
        for n in 0..3 {
            let mut body = f.body();
            body.as_object_mut().unwrap().remove("lane_id");
            body["project"] = json!("lane-one");
            body["environment"] = json!("test");
            let (status, request) = f.submit("alice", &format!("long-{n}"), body).await;
            assert_eq!(status, StatusCode::ACCEPTED);
            ids.push(request["id"].clone());
        }
        let (status, first) = f.call("alice", "GET", "/v1/requests?limit=1", None).await;
        assert_eq!(status, StatusCode::OK);
        let cursor = first["next_cursor"].as_str().unwrap();
        assert_eq!(cursor.len(), 35);
        let (status, second) = f
            .call(
                "alice",
                "GET",
                &format!("/v1/requests?limit=1&cursor={cursor}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(second["items"][0]["id"], ids[1]);
        let (denied_status, denied) = f
            .call(
                "outside",
                "GET",
                &format!("/v1/requests?cursor={cursor}"),
                None,
            )
            .await;
        let (missing_status, missing) = f
            .call(
                "outside",
                "GET",
                &format!("/v1/requests?cursor=v1.{}", Uuid::new_v4().simple()),
                None,
            )
            .await;
        assert_eq!(denied_status, StatusCode::BAD_REQUEST);
        assert_eq!(missing_status, denied_status);
        assert_eq!(denied, missing);
        assert_eq!(denied["code"], "invalid_cursor");
    }

    #[tokio::test]
    async fn bearer_scope_and_urgent_authorization_are_enforced() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-b", "release"]);
        git(&work, &["config", "user.name", "Shipbraid Test"]);
        git(&work, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(work.join("app.txt"), "synthetic fixture\n").unwrap();
        git(&work, &["add", "app.txt"]);
        git(&work, &["commit", "-m", "fixture"]);
        let revision = git(&work, &["rev-parse", "HEAD"]);
        let mirror = temp.path().join("source.git");
        let clone = Command::new("git")
            .args(["clone", "--mirror"])
            .arg(&work)
            .arg(&mirror)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert!(
            clone.status.success(),
            "git mirror clone failed: {}",
            String::from_utf8_lossy(&clone.stderr)
        );

        let token = format!("scoped-{}-credential-long-enough", Uuid::new_v4());
        let token_env = format!("SHIPBRAID_TEST_TOKEN_{}", Uuid::new_v4().simple());
        // Rust 2024 marks process environment mutation unsafe; the test uses
        // a unique variable name and removes it immediately after resolution.
        unsafe { std::env::set_var(&token_env, &token) };
        let lane = |id: &str, project: &str| LaneConfig {
            id: id.to_owned(),
            project: project.to_owned(),
            environment: "test".into(),
            repository_id: "repo".into(),
            repository_path: PathBuf::from(&mirror),
            release_ref: "refs/heads/release".into(),
            profile_digest: "synthetic-profile".into(),
            components: vec!["app".into()],
            allow_descendant: true,
            allow_preemption: true,
            failure_observe: None,
            request_parameters: Default::default(),
            enabled: true,
            executor: LaneExecutorConfig::Scripted,
        };
        let config = Config {
            database_path: temp.path().join("state.db"),
            tokens: vec![TokenConfig {
                id: "lane-one-operator".into(),
                token_env: token_env.clone(),
                lanes: vec!["lane-one".into()],
                projects: vec![],
                urgent: false,
                admin: false,
                executor: false,
            }],
            lanes: vec![
                lane("lane-one", "project-one"),
                lane("lane-two", "project-two"),
            ],
            ..Config::default()
        };
        let service = Service::open(config).await.unwrap();
        unsafe { std::env::remove_var(&token_env) };
        let app = router(service);

        let no_auth = send(app.clone(), "GET", "/v1/lanes", None, None).await;
        assert_eq!(no_auth.status(), StatusCode::UNAUTHORIZED);
        let denied_lane = send(app.clone(), "GET", "/v1/lanes/lane-two", Some(&token), None).await;
        assert_eq!(denied_lane.status(), StatusCode::FORBIDDEN);
        let denied_environment = send(
            app.clone(),
            "GET",
            "/v1/lanes?project=project-two&environment=test",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(denied_environment.status(), StatusCode::FORBIDDEN);

        let urgent = format!(
            "{{\"lane_id\":\"lane-one\",\"revision\":\"{revision}\",\"components\":[\"app\"],\"coverage_mode\":\"exact\",\"priority\":\"urgent\",\"reason\":\"test\"}}"
        );
        let denied_urgent = send(
            app.clone(),
            "POST",
            "/v1/requests",
            Some(&token),
            Some(&urgent),
        )
        .await;
        assert_eq!(denied_urgent.status(), StatusCode::FORBIDDEN);

        let normal = format!(
            "{{\"lane_id\":\"lane-one\",\"revision\":\"{revision}\",\"components\":[\"app\"],\"coverage_mode\":\"exact\",\"priority\":\"normal\",\"reason\":\"test\"}}"
        );
        let accepted = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/requests")
                    .header(AUTHORIZATION, format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .header("idempotency-key", "api-test-key")
                    .body(Body::from(normal))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::ACCEPTED);
        let payload: Value =
            serde_json::from_slice(&accepted.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert!(payload["id"].as_str().is_some());
        assert_eq!(payload["lane_id"], "lane-one");

        let alternate = format!(
            "{{\"project\":\"project-one\",\"environment\":\"test\",\"revision\":\"{revision}\",\"components\":[\"app\"],\"coverage_mode\":\"exact\",\"priority\":\"normal\",\"reason\":\"test\"}}"
        );
        let reused = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/requests")
                    .header(AUTHORIZATION, format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .header("idempotency-key", "api-test-key")
                    .body(Body::from(alternate))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reused.status(), StatusCode::ACCEPTED);
        let reused: Value =
            serde_json::from_slice(&reused.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(reused["id"], payload["id"]);
        assert_eq!(reused["reused"], true);
    }
    #[tokio::test]
    async fn project_parameters_are_required_frozen_and_part_of_idempotency() {
        let schema = [(
            "database_backup".to_owned(),
            crate::domain::ParameterSpec {
                required: true,
                allowed_values: vec!["include".into(), "skip".into()],
            },
        )]
        .into_iter()
        .collect();
        let f = team_fixture_with_parameters("lane-one", schema).await;
        let before = f.service.snapshot().await;
        for values in [
            json!({}),
            json!({"database_backup":"auto"}),
            json!({"database_backup":"skip", "command":"arbitrary"}),
        ] {
            let mut body = f.body();
            body["parameters"] = values;
            let (status, _) = f.submit("alice", "invalid-parameters", body).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        }
        assert_eq!(
            f.service.snapshot().await.requests.len(),
            before.requests.len()
        );
        assert!(f.service.snapshot().await.runs.is_empty());
        let mut body = f.body();
        body["parameters"] = json!({"database_backup":"skip"});
        let (status, first) = f.submit("alice", "explicit-policy", body.clone()).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(first["parameters"]["database_backup"], "skip");
        let (status, replay) = f.submit("alice", "explicit-policy", body.clone()).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(first["id"], replay["id"]);
        body["parameters"]["database_backup"] = json!("include");
        assert_eq!(
            f.submit("alice", "explicit-policy", body).await.0,
            StatusCode::CONFLICT
        );
        let id = Uuid::parse_str(first["id"].as_str().unwrap()).unwrap();
        assert_eq!(
            f.service.snapshot().await.requests[&id].target.parameters["database_backup"],
            "skip"
        );
        let (status, lane) = f.call("alice", "GET", "/v1/lanes/lane-one", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            lane["request_parameters"]["database_backup"]["required"],
            true
        );
    }
}
