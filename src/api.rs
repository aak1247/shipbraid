use std::{collections::BTreeSet, sync::Arc};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State, rejection::JsonRejection},
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
        CoverageMode, DeliveryRun, Event, Lane, Phase, Priority, ReleaseRequest,
        State as DomainState,
    },
    service::{Service, ServiceError},
};

const MAX_REQUEST_BODY: usize = 16 * 1024;
const MAX_REASON_BYTES: usize = 2 * 1024;
const MAX_LIST_LIMIT: usize = 200;

#[derive(Clone)]
struct ApiState {
    service: Arc<Service>,
    tokens: Vec<ResolvedToken>,
}

#[derive(Clone)]
struct Actor {
    id: String,
    lanes: Vec<String>,
    projects: Vec<String>,
    urgent: bool,
    admin: bool,
    executor: bool,
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

async fn list_requests(
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
    let mut requests: Vec<_> = snapshot
        .requests
        .values()
        .filter(|request| {
            lane_filter
                .as_ref()
                .is_none_or(|lanes| lanes.contains(&request.lane_id))
        })
        .collect();
    requests.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.creation_sequence.cmp(&left.creation_sequence))
    });
    let items = requests
        .into_iter()
        .map(|request| request_view(request, &snapshot))
        .collect();
    Ok(Json(Items { items }))
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
    Ok(Json(request_view(request, &snapshot)))
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
    let body_value = json!({
        "lane_id": &lane.id,
        "revision": &body.revision,
        "components": &components,
        "coverage_mode": &body.coverage_mode,
        "priority": &body.priority,
        "deadline_at": &body.deadline_at,
        "reason": &body.reason,
    });
    let request_hash = hex::encode(Sha256::digest(
        serde_json::to_vec(&body_value).unwrap_or_default(),
    ));
    let outcome = state
        .service
        .submit(
            actor.id,
            idempotency_key.to_owned(),
            request_hash,
            lane.id,
            body.revision,
            body.components,
            body.coverage_mode,
            body.priority,
            body.deadline_at,
            body.reason,
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
    let mut view = request_view(request, &snapshot);
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
    state.service.withdraw(id, actor.id).await?;
    let snapshot = state.service.snapshot().await;
    let request = snapshot
        .requests
        .get(&id)
        .ok_or_else(|| ApiError::missing("Request not found."))?;
    Ok(Json(request_view(request, &snapshot)))
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
    state.service.retry(id).await?;
    let snapshot = state.service.snapshot().await;
    let request = snapshot
        .requests
        .get(&id)
        .ok_or_else(|| ApiError::missing("Request not found."))?;
    Ok(Json(request_view(request, &snapshot)))
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
        .set_priority(id, body.priority, reason)
        .await?;
    let snapshot = state.service.snapshot().await;
    let request = snapshot
        .requests
        .get(&id)
        .ok_or_else(|| ApiError::missing("Request not found."))?;
    Ok(Json(request_view(request, &snapshot)))
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
    actor.admin
        || actor.lanes.iter().any(|id| id == &lane.id)
        || actor
            .projects
            .iter()
            .any(|project| project == &lane.project)
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

fn request_view(request: &ReleaseRequest, snapshot: &DomainState) -> Value {
    let receipt = request
        .satisfied_by_receipt
        .and_then(|id| snapshot.receipts.get(&id));
    json!({
        "id": request.id,
        "lane_id": request.lane_id,
        "revision": request.target.revision,
        "components": request.target.components,
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

    #[tokio::test]
    async fn bearer_scope_and_urgent_authorization_are_enforced() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-b", "release"]);
        git(&work, &["config", "user.name", "Auto Deliver Test"]);
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
        let token_env = format!("AUTO_DELIVER_TEST_TOKEN_{}", Uuid::new_v4().simple());
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
}
