use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

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
    routing::{get, post, put},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    config::{GuideDocument, ResolvedToken},
    domain::{
        CoverageMode, DeliveryRun, Event, LOCAL_TENANT_ID, Lane, Phase, Priority, ReleaseRequest,
        RequestContext, RequestStatus, State as DomainState,
    },
    resources::{
        ManagedResource, ResourceEvent, ResourceGcPlan, ResourcePin, ResourceRegistration,
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
            ServiceError::InvalidResource(message) => {
                Self::bad_request("invalid_resource", &message)
            }
            ServiceError::ResourceConflict(message) => {
                Self::new(StatusCode::CONFLICT, "resource_conflict", message, false)
            }
            ServiceError::ResourceNotFound => Self::missing("Resource not found."),
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
    run_id: Uuid,
    items: Vec<LogItem>,
    next_cursor: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestLogQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct RequestLogPage {
    request_id: Uuid,
    run_id: Option<Uuid>,
    items: Vec<LogItem>,
    next_cursor: Option<String>,
}

struct RequestLogCursor {
    request_id: Uuid,
    run_id: Uuid,
    sequence: u64,
}

impl RequestLogCursor {
    fn encode(&self) -> String {
        format!(
            "v1.{}.{}.{}",
            self.request_id.simple(),
            self.run_id.simple(),
            self.sequence
        )
    }

    fn decode(value: &str, request_id: Uuid) -> Result<Self, ApiError> {
        let invalid = || {
            ApiError::bad_request(
                "invalid_cursor",
                "cursor must be a request-log cursor for this request.",
            )
        };
        if value.len() > 100 {
            return Err(invalid());
        }
        let mut parts = value.split('.');
        if parts.next() != Some("v1") {
            return Err(invalid());
        }
        let cursor = Self {
            request_id: Uuid::parse_str(parts.next().ok_or_else(invalid)?)
                .map_err(|_| invalid())?,
            run_id: Uuid::parse_str(parts.next().ok_or_else(invalid)?).map_err(|_| invalid())?,
            sequence: parts
                .next()
                .ok_or_else(invalid)?
                .parse()
                .map_err(|_| invalid())?,
        };
        if parts.next().is_some() || cursor.request_id != request_id {
            return Err(invalid());
        }
        Ok(cursor)
    }
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
    let artifacts_dir = service.artifacts_dir().map(std::path::Path::to_path_buf);
    let state = ApiState {
        tokens: service.tokens().to_vec(),
        service,
    };
    let protected = Router::new()
        .route("/system", get(system))
        .route("/artifacts", get(list_artifacts))
        .route("/resources", get(list_resources))
        .route("/resources/gc-plan", get(resource_gc_plan))
        .route("/resources/{resource_id}", put(register_resource))
        .route(
            "/resources/{resource_id}/pins",
            put(add_resource_pin).delete(remove_resource_pin),
        )
        .route("/resources/{resource_id}/events", get(resource_events))
        .route("/lanes", get(list_lanes))
        .route("/lanes/{lane_id}", get(get_lane))
        .route("/lanes/{lane_id}/guide", get(get_lane_guide))
        .route("/lanes/{lane_id}/unpause", post(unpause_lane))
        .route("/lanes/{lane_id}/resolve-unknown", post(resolve_unknown))
        .route("/guides/{project}", get(get_project_guide))
        .route("/requests", get(list_requests).post(create_request))
        .route("/requests/{request_id}", get(get_request))
        .route("/requests/{request_id}/logs", get(request_logs))
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

    if let Some(path) = artifacts_dir {
        router = router.nest_service(
            "/artifacts",
            tower_http::services::ServeDir::new(path).append_index_html_on_directories(true),
        );
    }

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
        capabilities: json!({"submit":true,"withdraw":true,"retry":true,"raise_priority":actor.urgent || actor.admin,"admin":actor.admin,"unpause":actor.admin}),
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
struct LaneGuideResponse {
    lane_id: String,
    project: String,
    environment: String,
    sections: Vec<LaneGuideSection>,
}

#[derive(Debug, Serialize)]
struct LaneGuideSection {
    scope: &'static str,
    name: String,
    content_sha256: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ProjectGuideResponse {
    project: String,
    overview: Option<GuideContent>,
    environments: Vec<ProjectEnvironmentGuide>,
    components: Vec<ProjectComponentGuide>,
}

#[derive(Clone, Debug, Serialize)]
struct GuideContent {
    content_sha256: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ProjectEnvironmentGuide {
    environment: String,
    lane_ids: Vec<String>,
    content_sha256: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ProjectComponentGuide {
    component: String,
    content_sha256: String,
    content: String,
}

async fn get_lane_guide(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(lane_id): Path<String>,
) -> Result<Json<LaneGuideResponse>, ApiError> {
    require_non_executor(&actor)?;
    let snapshot = state.service.snapshot().await;
    let lane = snapshot
        .lanes
        .get(&lane_id)
        .ok_or_else(|| ApiError::missing("Lane not found."))?;
    require_lane(&actor, lane)?;

    let mut sections = Vec::new();
    if let Some(guide) = state.service.lane_guide(&lane_id) {
        if let Some(doc) = &guide.project {
            sections.push(LaneGuideSection {
                scope: "project",
                name: "project".into(),
                content_sha256: doc.content_sha256.clone(),
                content: doc.content.clone(),
            });
        }
        if let Some(doc) = &guide.environment {
            sections.push(LaneGuideSection {
                scope: "environment",
                name: lane.environment.clone(),
                content_sha256: doc.content_sha256.clone(),
                content: doc.content.clone(),
            });
        }
        if let Some(declared) = state.service.lane_declared_components(&lane_id) {
            for comp in declared {
                if let Some(doc) = guide.components.get(comp) {
                    sections.push(LaneGuideSection {
                        scope: "component",
                        name: comp.clone(),
                        content_sha256: doc.content_sha256.clone(),
                        content: doc.content.clone(),
                    });
                }
            }
        }
    }

    Ok(Json(LaneGuideResponse {
        lane_id: lane.id.clone(),
        project: lane.project.clone(),
        environment: lane.environment.clone(),
        sections,
    }))
}

async fn get_project_guide(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(project): Path<String>,
) -> Result<Json<ProjectGuideResponse>, ApiError> {
    require_non_executor(&actor)?;
    let snapshot = state.service.snapshot().await;
    let mut visible_lanes: Vec<&Lane> = snapshot
        .lanes
        .values()
        .filter(|lane| lane.project == project && lane_allowed(&actor, lane))
        .collect();
    if visible_lanes.is_empty() {
        return Err(ApiError::forbidden());
    }
    visible_lanes.sort_by(|a, b| a.id.cmp(&b.id));

    let mut overview = None;
    let mut env_entries = BTreeMap::<PathBuf, (String, Vec<String>, GuideDocument)>::new();
    let mut comp_entries = BTreeMap::<PathBuf, (String, GuideDocument)>::new();

    for lane in &visible_lanes {
        if let Some(guide) = state.service.lane_guide(&lane.id) {
            if overview.is_none()
                && let Some(doc) = &guide.project
            {
                overview = Some(GuideContent {
                    content_sha256: doc.content_sha256.clone(),
                    content: doc.content.clone(),
                });
            }
            if let Some(doc) = &guide.environment {
                env_entries
                    .entry(doc.path.clone())
                    .and_modify(|(_, lane_ids, _)| {
                        if !lane_ids.contains(&lane.id) {
                            lane_ids.push(lane.id.clone());
                        }
                    })
                    .or_insert_with(|| {
                        (lane.environment.clone(), vec![lane.id.clone()], doc.clone())
                    });
            }
            if let Some(declared) = state.service.lane_declared_components(&lane.id) {
                for comp in declared {
                    if let Some(doc) = guide.components.get(comp) {
                        comp_entries
                            .entry(doc.path.clone())
                            .or_insert_with(|| (comp.clone(), doc.clone()));
                    }
                }
            }
        }
    }

    let mut environments: Vec<ProjectEnvironmentGuide> = env_entries
        .into_values()
        .map(|(environment, mut lane_ids, doc)| {
            lane_ids.sort();
            ProjectEnvironmentGuide {
                environment,
                lane_ids,
                content_sha256: doc.content_sha256,
                content: doc.content,
            }
        })
        .collect();
    environments.sort_by(|a, b| a.environment.cmp(&b.environment));

    let mut components: Vec<ProjectComponentGuide> = comp_entries
        .into_values()
        .map(|(component, doc)| ProjectComponentGuide {
            component,
            content_sha256: doc.content_sha256,
            content: doc.content,
        })
        .collect();
    components.sort_by(|a, b| a.component.cmp(&b.component));

    Ok(Json(ProjectGuideResponse {
        project,
        overview,
        environments,
        components,
    }))
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

async fn request_logs(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(request_id): Path<String>,
    query: Result<Query<RequestLogQuery>, QueryRejection>,
) -> Result<Json<RequestLogPage>, ApiError> {
    require_non_executor(&actor)?;
    let id = parse_uuid(&request_id)?;
    // Resolve the request and read its run/logs from one consistent snapshot.
    let snapshot = state.service.snapshot().await;
    let request = snapshot
        .requests
        .get(&id)
        .ok_or_else(|| ApiError::missing("Request not found."))?;
    require_lane_id(&actor, &request.lane_id, &snapshot)?;
    let Query(query) = query.map_err(|_| {
        ApiError::bad_request(
            "invalid_query",
            "Supply cursor and an optional numeric limit.",
        )
    })?;
    let cursor = query
        .cursor
        .as_deref()
        .map(|cursor| RequestLogCursor::decode(cursor, id))
        .transpose()?;
    let run_id = request
        .current_run_id
        .or(request.satisfied_by_run)
        .or_else(|| {
            snapshot.events.iter().rev().find_map(|event| {
                (event.request_id == Some(id) && event.lane_id == request.lane_id)
                    .then_some(event.run_id)
                    .flatten()
            })
        });
    let Some(run_id) = run_id else {
        return Ok(Json(RequestLogPage {
            request_id: id,
            run_id: None,
            items: vec![],
            next_cursor: None,
        }));
    };
    let run = snapshot
        .runs
        .get(&run_id)
        .ok_or_else(|| ApiError::missing("Run not found."))?;
    require_lane_id(&actor, &run.lane_id, &snapshot)?;
    let after = cursor
        .filter(|cursor| cursor.run_id == run_id)
        .map_or(0, |cursor| cursor.sequence);
    let page = log_page(&snapshot, run_id, after, bounded_limit(query.limit));
    Ok(Json(RequestLogPage {
        request_id: id,
        run_id: Some(run_id),
        items: page.items,
        next_cursor: Some(
            RequestLogCursor {
                request_id: id,
                run_id,
                sequence: page.next_cursor,
            }
            .encode(),
        ),
    }))
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
    Ok(Json(log_page(
        &snapshot,
        id,
        query.after.unwrap_or_default(),
        bounded_limit(query.limit),
    )))
}

fn log_page(snapshot: &DomainState, id: Uuid, after: u64, limit: usize) -> LogPage {
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
    LogPage {
        run_id: id,
        items,
        next_cursor,
    }
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

async fn unpause_lane(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(lane_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&actor)?;
    let snapshot = state.service.snapshot().await;
    let lane = snapshot
        .lanes
        .get(&lane_id)
        .ok_or_else(|| ApiError::missing("Lane not found."))?;
    require_lane(&actor, lane)?;
    state
        .service
        .unpause_lane(&lane_id, Some(&actor.id))
        .await?;
    let snapshot = state.service.snapshot().await;
    let lane = snapshot
        .lanes
        .get(&lane_id)
        .ok_or_else(|| ApiError::missing("Lane not found."))?;
    Ok(Json(lane_view(lane, state.service.executor_kind(&lane.id))))
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

#[derive(Debug, Serialize)]
struct ArtifactItem {
    path: String,
    filename: String,
    size_bytes: u64,
    download_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    modified_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
struct ArtifactsResponse {
    artifacts: Vec<ArtifactItem>,
}

async fn list_artifacts(
    State(state): State<ApiState>,
) -> Result<Json<ArtifactsResponse>, ApiError> {
    let Some(artifacts_dir) = state.service.artifacts_dir() else {
        return Ok(Json(ArtifactsResponse { artifacts: vec![] }));
    };
    if !artifacts_dir.is_dir() {
        return Ok(Json(ArtifactsResponse { artifacts: vec![] }));
    }

    let mut artifacts = Vec::new();
    collect_artifacts(artifacts_dir, artifacts_dir, &mut artifacts);
    artifacts.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(Json(ArtifactsResponse { artifacts }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceEventQuery {
    after: Option<u64>,
    limit: Option<usize>,
}

async fn list_resources(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
) -> Result<Json<Items<ManagedResource>>, ApiError> {
    require_admin(&actor)?;
    Ok(Json(Items {
        items: state.service.list_resources().await?,
    }))
}

async fn resource_gc_plan(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
) -> Result<Json<ResourceGcPlan>, ApiError> {
    require_admin(&actor)?;
    Ok(Json(state.service.resource_gc_plan().await?))
}

async fn register_resource(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(resource_id): Path<String>,
    body: Result<Json<ResourceRegistration>, JsonRejection>,
) -> Result<Json<ManagedResource>, ApiError> {
    require_admin(&actor)?;
    let resource = state
        .service
        .register_resource(resource_id, parse_body(body)?, &actor.id)
        .await?;
    Ok(Json(resource))
}

async fn add_resource_pin(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(resource_id): Path<String>,
    body: Result<Json<ResourcePin>, JsonRejection>,
) -> Result<Json<ManagedResource>, ApiError> {
    require_admin(&actor)?;
    Ok(Json(
        state
            .service
            .set_resource_pin(&resource_id, parse_body(body)?, true, &actor.id)
            .await?,
    ))
}

async fn remove_resource_pin(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(resource_id): Path<String>,
    body: Result<Json<ResourcePin>, JsonRejection>,
) -> Result<Json<ManagedResource>, ApiError> {
    require_admin(&actor)?;
    Ok(Json(
        state
            .service
            .set_resource_pin(&resource_id, parse_body(body)?, false, &actor.id)
            .await?,
    ))
}

async fn resource_events(
    State(state): State<ApiState>,
    axum::extract::Extension(actor): axum::extract::Extension<Actor>,
    Path(resource_id): Path<String>,
    query: Result<Query<ResourceEventQuery>, QueryRejection>,
) -> Result<Json<Items<ResourceEvent>>, ApiError> {
    require_admin(&actor)?;
    let Query(query) = query.map_err(|_| {
        ApiError::bad_request(
            "invalid_query",
            "Resource event query parameters are invalid.",
        )
    })?;
    let limit = query.limit.unwrap_or(100);
    if !(1..=MAX_LIST_LIMIT).contains(&limit) {
        return Err(ApiError::bad_request(
            "invalid_limit",
            "limit must be between 1 and 200.",
        ));
    }
    Ok(Json(Items {
        items: state
            .service
            .resource_events(&resource_id, query.after.unwrap_or_default(), limit)
            .await?,
    }))
}

fn collect_artifacts(
    root: &std::path::Path,
    current: &std::path::Path,
    out: &mut Vec<ArtifactItem>,
) {
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().to_string();
        if file_name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_artifacts(root, &path, out);
        } else if path.is_file() {
            let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let modified_at = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(DateTime::<Utc>::from);
            if let Ok(rel) = path.strip_prefix(root) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                let download_url = format!("/artifacts/{}", rel_str);
                out.push(ArtifactItem {
                    path: rel_str,
                    filename: file_name,
                    size_bytes,
                    download_url,
                    modified_at,
                });
            }
        }
    }
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
        resources::ResourceRootConfig,
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
        resource_root: PathBuf,
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
        let resource_root = temp.path().join("managed-resources");
        std::fs::create_dir(&resource_root).unwrap();
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
            resource_admission: None,
            request_parameters: parameters.clone(),
            enabled: true,
            executor: LaneExecutorConfig::Scripted,
            guide: None,
            credentials: None,
            hook_environment: None,
        };
        let service = Service::open(Config {
            database_path: temp.path().join("ledger.db"),
            tokens: configured,
            lanes: vec![lane(lane_one_id, "lane-one"), lane("lane-two", "lane-two")],
            resource_roots: vec![ResourceRootConfig {
                id: "build-cache".into(),
                path: resource_root.clone(),
                managed_budget_bytes: 1,
                minimum_free_bytes: 0,
            }],
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
            resource_root,
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
    async fn resource_routes_are_admin_only_scoped_and_read_only() {
        let f = team_fixture().await;
        let resource_path = f.resource_root.join("brotli-v1");
        std::fs::create_dir(&resource_path).unwrap();
        std::fs::create_dir(resource_path.join("nested")).unwrap();
        let path = "/v1/resources/autotask-brotli-v1";
        let body = json!({
            "root_id": "build-cache",
            "relative_path": "brotli-v1",
            "owner": "autotask",
            "project": "shared-cache",
            "kind": "build_cache",
            "input_digest": "sha256:abc123",
            "toolchain": "rustc-1.98",
            "platform": "linux-x86_64",
            "observed_bytes": 4096,
            "recreatable": true
        });

        let (status, _) = f.call("alice", "PUT", path, Some(body.clone())).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, oversized) = f
            .call(
                "admin",
                "PUT",
                "/v1/resources/oversized-resource",
                Some(json!({"padding": "x".repeat(MAX_REQUEST_BODY)})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(oversized["code"], "invalid_json");
        let (status, registered) = f.call("admin", "PUT", path, Some(body.clone())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(registered["id"], "autotask-brotli-v1");
        assert_eq!(registered["root_id"], "build-cache");
        assert_eq!(
            registered["canonical_path"],
            resource_path.to_string_lossy().as_ref()
        );

        let mut overlap = body.clone();
        overlap["relative_path"] = json!("brotli-v1/nested");
        let (status, conflict) = f
            .call(
                "admin",
                "PUT",
                "/v1/resources/overlapping-resource",
                Some(overlap),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(conflict["code"], "resource_conflict");

        let mut traversal = body.clone();
        traversal["relative_path"] = json!("../source.git");
        let (status, invalid) = f
            .call(
                "admin",
                "PUT",
                "/v1/resources/traversal-resource",
                Some(traversal),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(invalid["code"], "invalid_resource");

        let pin = json!({"kind":"current_release","reference_id":"release-sha"});
        let (status, pinned) = f
            .call("admin", "PUT", &format!("{path}/pins"), Some(pin.clone()))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(pinned["pins"].as_array().unwrap().len(), 1);
        let (status, events) = f
            .call(
                "admin",
                "GET",
                "/v1/resources/autotask-brotli-v1/events?after=0&limit=10",
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(events["items"].as_array().unwrap().len(), 2);

        let (status, unpinned) = f
            .call("admin", "DELETE", &format!("{path}/pins"), Some(pin))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(unpinned["pins"].as_array().unwrap().len(), 0);
        let (status, plan) = f.call("admin", "GET", "/v1/resources/gc-plan", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(plan["read_only"], true);
        assert_eq!(plan["roots"][0]["selected"][0]["id"], "autotask-brotli-v1");
        assert!(resource_path.exists());
    }

    #[tokio::test]
    async fn request_logs_enforce_scope_and_reset_stale_run_cursors() {
        let f = team_fixture().await;
        let (_, request) = f.submit("alice", "request-logs", f.body()).await;
        let id = request["id"].as_str().unwrap();
        let path = format!("/v1/requests/{id}/logs");
        let (status, page) = f.call("bob", "GET", &path, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            page,
            json!({"request_id":id,"run_id":null,"items":[],"next_cursor":null})
        );
        for actor in ["outside", "executor"] {
            let (status, _) = f.call(actor, "GET", &path, None).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
        }
        let response = send(f.app.clone(), "GET", &path, None, None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        for query in [
            "cursor=bad",
            "cursor=1",
            "limit=bad",
            "limit=-1",
            "after=10",
        ] {
            let (status, _) = f
                .call("alice", "GET", &format!("{path}?{query}"), None)
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
        }
        let cursor = RequestLogCursor {
            request_id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            sequence: 1,
        }
        .encode();
        let (status, error) = f
            .call("alice", "GET", &format!("{path}?cursor={cursor}"), None)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["code"], "invalid_cursor");
        let (status, _) = f
            .call(
                "alice",
                "GET",
                &format!("/v1/requests/{}/logs", Uuid::new_v4()),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Let the fixture's coalescing window elapse, then dispatch once. No
        // scheduler runs in the background, so the log pages remain stable.
        tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
        f.service.scheduler_tick().await.unwrap();
        let (status, first) = f.call("alice", "GET", &path, None).await;
        assert_eq!(status, StatusCode::OK);
        let run_id = first["run_id"].as_str().unwrap();
        assert!(!first["items"].as_array().unwrap().is_empty());
        let stale = RequestLogCursor {
            request_id: Uuid::parse_str(id).unwrap(),
            run_id: Uuid::new_v4(),
            // A sequence above every log verifies that changing runs really
            // resets the cursor, even when the selected run has older logs.
            sequence: u64::MAX,
        }
        .encode();
        let (status, reset) = f
            .call("alice", "GET", &format!("{path}?cursor={stale}"), None)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(reset, first);
        let (_, fixed) = f
            .call("alice", "GET", &format!("/v1/runs/{run_id}/logs"), None)
            .await;
        assert_eq!(reset["items"], fixed["items"]);
        let (_, empty) = f
            .call(
                "alice",
                "GET",
                &format!("{path}?cursor={}", first["next_cursor"].as_str().unwrap()),
                None,
            )
            .await;
        assert_eq!(empty["items"], json!([]));
        assert_eq!(empty["next_cursor"], first["next_cursor"]);
    }

    #[test]
    fn request_log_cursors_are_bounded_and_request_scoped() {
        let request_id = Uuid::new_v4();
        let cursor = RequestLogCursor {
            request_id,
            run_id: Uuid::new_v4(),
            sequence: u64::MAX,
        };
        let encoded = cursor.encode();
        let decoded = RequestLogCursor::decode(&encoded, request_id).unwrap();
        assert_eq!(decoded.run_id, cursor.run_id);
        assert_eq!(decoded.sequence, u64::MAX);
        assert!(RequestLogCursor::decode(&encoded, Uuid::new_v4()).is_err());
        for value in [
            format!("{encoded}.extra"),
            encoded.replace("v1.", "v2."),
            "x".repeat(101),
        ] {
            assert!(RequestLogCursor::decode(&value, request_id).is_err());
        }
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
        assert_eq!(system["capabilities"]["unpause"], false);
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
            resource_admission: None,
            request_parameters: Default::default(),
            enabled: true,
            executor: LaneExecutorConfig::Scripted,
            guide: None,
            credentials: None,
            hook_environment: None,
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

    #[tokio::test]
    async fn artifacts_distribution_serves_files_and_lists_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let artifacts_path = temp.path().join("dist_artifacts");
        std::fs::create_dir_all(artifacts_path.join("project_a")).unwrap();
        std::fs::write(
            artifacts_path.join("project_a/binary.bin"),
            b"hello-artifact-distribution",
        )
        .unwrap();

        let mirror = temp.path().join("source.git");
        let output = std::process::Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&mirror)
            .output()
            .unwrap();
        assert!(output.status.success());

        let token_env = format!("TEST_TOKEN_{}", Uuid::new_v4().simple());
        let token = "a".repeat(40);
        unsafe { std::env::set_var(&token_env, &token) };

        let config = Config {
            database_path: temp.path().join("state.db"),
            tokens: vec![TokenConfig {
                id: "admin".into(),
                token_env: token_env.clone(),
                lanes: vec!["lane-one".into()],
                projects: vec![],
                urgent: true,
                admin: true,
                executor: false,
            }],
            lanes: vec![LaneConfig {
                id: "lane-one".into(),
                project: "project_a".into(),
                environment: "test".into(),
                repository_id: "repo".into(),
                repository_path: PathBuf::from(&mirror),
                release_ref: "refs/heads/release".into(),
                profile_digest: "synthetic-profile".into(),
                components: vec!["app".into()],
                allow_descendant: true,
                allow_preemption: true,
                failure_observe: None,
                resource_admission: None,
                request_parameters: Default::default(),
                enabled: true,
                executor: LaneExecutorConfig::Scripted,
                guide: None,
                credentials: None,
                hook_environment: None,
            }],
            artifacts_dir: Some(artifacts_path),
            ..Config::default()
        };
        let service = Service::open(config).await.unwrap();
        unsafe { std::env::remove_var(&token_env) };
        let app = router(service);

        // 1. Direct download from /artifacts without auth
        let resp = send(
            app.clone(),
            "GET",
            "/artifacts/project_a/binary.bin",
            None,
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"hello-artifact-distribution");

        // 2. Listing via /v1/artifacts with auth
        let (status, list_json) = {
            let resp = send(app.clone(), "GET", "/v1/artifacts", Some(&token), None).await;
            response_json(resp).await
        };
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list_json["artifacts"].as_array().unwrap().len(), 1);
        assert_eq!(list_json["artifacts"][0]["path"], "project_a/binary.bin");
        assert_eq!(
            list_json["artifacts"][0]["download_url"],
            "/artifacts/project_a/binary.bin"
        );
        assert_eq!(list_json["artifacts"][0]["size_bytes"], 27);
    }

    #[tokio::test]
    async fn unpause_lane_requires_admin_and_clears_failure_paused() {
        let f = team_fixture().await;

        // Verify capabilities for admin vs non-admin
        let (_, admin_system) = f.call("admin", "GET", "/v1/system", None).await;
        assert_eq!(admin_system["capabilities"]["unpause"], true);
        let (_, bob_system) = f.call("bob", "GET", "/v1/system", None).await;
        assert_eq!(bob_system["capabilities"]["unpause"], false);

        // Non-admin cannot call unpause
        assert_eq!(
            f.call("bob", "POST", "/v1/lanes/lane-one/unpause", None)
                .await
                .0,
            StatusCode::FORBIDDEN
        );

        // Admin calling nonexistent lane returns 404
        assert_eq!(
            f.call("admin", "POST", "/v1/lanes/nonexistent/unpause", None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );

        // Put lane-one into failure_paused
        f.service
            .transact(Some("lane-one"), |_, state, _| {
                let lane = state.lanes.get_mut("lane-one").unwrap();
                lane.failure_paused = true;
                lane.blocked_reason = Some("failure_paused".to_owned());
                Ok(())
            })
            .await
            .unwrap();
        assert!(f.service.snapshot().await.lanes["lane-one"].failure_paused);

        // Admin successfully unpauses
        let (status, lane) = f
            .call("admin", "POST", "/v1/lanes/lane-one/unpause", None)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(lane["failure_paused"], false);
        assert_eq!(lane["blocked_reason"], Value::Null);
        let snapshot = f.service.snapshot().await;
        assert!(!snapshot.lanes["lane-one"].failure_paused);
        assert_eq!(snapshot.lanes["lane-one"].blocked_reason, None);
        let unpaused_event = snapshot
            .events
            .iter()
            .find(|e| e.kind == "lane_unpaused" && e.lane_id == "lane-one")
            .unwrap();
        assert_eq!(unpaused_event.actor.as_deref(), Some("admin"));
    }

    #[tokio::test]
    async fn withdrawing_failed_request_via_api_unpauses_lane() {
        let f = team_fixture().await;
        let mut body = f.body();
        body["lane_id"] = json!("lane-one");
        let (status, request) = f.submit("alice", "fail-and-withdraw", body).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let request_id = parse_uuid(request["id"].as_str().unwrap()).unwrap();

        // Simulate run failure settling into NeedsAttention and failure_paused
        f.service
            .transact(Some("lane-one"), |_, state, _| {
                let req = state.requests.get_mut(&request_id).unwrap();
                req.status = RequestStatus::NeedsAttention;
                req.waiting_reason = Some("run_failed".to_owned());
                let lane = state.lanes.get_mut("lane-one").unwrap();
                lane.failure_paused = true;
                lane.blocked_reason = Some("failure_paused".to_owned());
                lane.active_run_id = None;
                Ok(())
            })
            .await
            .unwrap();
        assert!(f.service.snapshot().await.lanes["lane-one"].failure_paused);

        // Alice withdraws the failed request
        let (status, view) = f
            .call(
                "alice",
                "POST",
                &format!("/v1/requests/{request_id}/withdraw"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(view["status"], "withdrawn");

        // Lane is automatically unpaused
        let snapshot = f.service.snapshot().await;
        assert!(!snapshot.lanes["lane-one"].failure_paused);
        assert_eq!(snapshot.lanes["lane-one"].blocked_reason, None);
        assert!(
            snapshot
                .events
                .iter()
                .any(|e| e.kind == "lane_unpaused" && e.lane_id == "lane-one")
        );
    }

    #[tokio::test]
    async fn lane_guide_and_project_guide_api_endpoints_work_with_auth_and_deduplication() {
        let temp = tempfile::tempdir().unwrap();
        let mirror = temp.path().join("source.git");
        let output = std::process::Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&mirror)
            .output()
            .unwrap();
        assert!(output.status.success());

        // Guide files
        let guides_dir = temp.path().join("guides");
        std::fs::create_dir_all(&guides_dir).unwrap();
        let overview_file = guides_dir.join("overview.md");
        std::fs::write(&overview_file, "# Project Overview\nGeneral guidelines.").unwrap();
        let dev_env_file = guides_dir.join("dev.md");
        std::fs::write(&dev_env_file, "# Dev Environment\nLocal dev guidelines.").unwrap();
        let prod_env_file = guides_dir.join("prod.md");
        std::fs::write(
            &prod_env_file,
            "# Prod Environment\nStrict production guidelines.",
        )
        .unwrap();
        let app_comp_file = guides_dir.join("app.md");
        std::fs::write(&app_comp_file, "# App Component\nApp guidelines.").unwrap();
        let worker_comp_file = guides_dir.join("worker.md");
        std::fs::write(&worker_comp_file, "# Worker Component\nWorker guidelines.").unwrap();

        let dev_token_env = format!("SHIPBRAID_TEST_DEV_{}", Uuid::new_v4().simple());
        let dev_token = "dev-operator-credential-32-bytes-long";
        unsafe { std::env::set_var(&dev_token_env, dev_token) };

        let other_token_env = format!("SHIPBRAID_TEST_OTHER_{}", Uuid::new_v4().simple());
        let other_token = "other-operator-credential-32-bytes-long";
        unsafe { std::env::set_var(&other_token_env, other_token) };

        let config = Config {
            database_path: temp.path().join("state.db"),
            tokens: vec![
                TokenConfig {
                    id: "dev-operator".into(),
                    token_env: dev_token_env.clone(),
                    lanes: vec!["myapp-dev".into(), "myapp-dev-canary".into()],
                    projects: vec![],
                    urgent: false,
                    admin: false,
                    executor: false,
                },
                TokenConfig {
                    id: "other-operator".into(),
                    token_env: other_token_env.clone(),
                    lanes: vec!["other-lane".into()],
                    projects: vec![],
                    urgent: false,
                    admin: false,
                    executor: false,
                },
            ],
            lanes: vec![
                LaneConfig {
                    id: "myapp-dev".into(),
                    project: "myapp".into(),
                    environment: "dev".into(),
                    repository_id: "repo".into(),
                    repository_path: mirror.clone(),
                    release_ref: "refs/heads/main".into(),
                    profile_digest: "synthetic-profile".into(),
                    components: vec!["worker".into(), "app".into()],
                    allow_descendant: false,
                    allow_preemption: false,
                    failure_observe: None,
                    resource_admission: None,
                    request_parameters: Default::default(),
                    enabled: true,
                    executor: LaneExecutorConfig::Scripted,
                    guide: Some(crate::config::GuideConfig {
                        project: Some(overview_file.clone()),
                        environment: Some(dev_env_file.clone()),
                        components: BTreeMap::from([
                            ("app".into(), app_comp_file.clone()),
                            ("worker".into(), worker_comp_file.clone()),
                        ]),
                    }),
                    credentials: None,
                    hook_environment: None,
                },
                LaneConfig {
                    id: "myapp-dev-canary".into(),
                    project: "myapp".into(),
                    environment: "dev".into(),
                    repository_id: "repo".into(),
                    repository_path: mirror.clone(),
                    release_ref: "refs/heads/main".into(),
                    profile_digest: "synthetic-profile".into(),
                    components: vec!["app".into()],
                    allow_descendant: false,
                    allow_preemption: false,
                    failure_observe: None,
                    resource_admission: None,
                    request_parameters: Default::default(),
                    enabled: true,
                    executor: LaneExecutorConfig::Scripted,
                    guide: Some(crate::config::GuideConfig {
                        project: Some(overview_file.clone()),
                        environment: Some(dev_env_file.clone()),
                        components: BTreeMap::from([("app".into(), app_comp_file.clone())]),
                    }),
                    credentials: None,
                    hook_environment: None,
                },
                LaneConfig {
                    id: "myapp-prod".into(),
                    project: "myapp".into(),
                    environment: "prod".into(),
                    repository_id: "repo".into(),
                    repository_path: mirror.clone(),
                    release_ref: "refs/heads/main".into(),
                    profile_digest: "synthetic-profile".into(),
                    components: vec!["app".into()],
                    allow_descendant: false,
                    allow_preemption: false,
                    failure_observe: None,
                    resource_admission: None,
                    request_parameters: Default::default(),
                    enabled: true,
                    executor: LaneExecutorConfig::Scripted,
                    guide: Some(crate::config::GuideConfig {
                        project: Some(overview_file.clone()),
                        environment: Some(prod_env_file.clone()),
                        components: BTreeMap::from([("app".into(), app_comp_file.clone())]),
                    }),
                    credentials: None,
                    hook_environment: None,
                },
                LaneConfig {
                    id: "other-lane".into(),
                    project: "other-project".into(),
                    environment: "test".into(),
                    repository_id: "repo".into(),
                    repository_path: mirror.clone(),
                    release_ref: "refs/heads/main".into(),
                    profile_digest: "synthetic-profile".into(),
                    components: vec!["app".into()],
                    allow_descendant: false,
                    allow_preemption: false,
                    failure_observe: None,
                    resource_admission: None,
                    request_parameters: Default::default(),
                    enabled: true,
                    executor: LaneExecutorConfig::Scripted,
                    guide: None,
                    credentials: None,
                    hook_environment: None,
                },
            ],
            ..Config::default()
        };

        let service = Service::open(config).await.unwrap();
        unsafe {
            std::env::remove_var(&dev_token_env);
            std::env::remove_var(&other_token_env);
        }
        let app = router(service);

        // 1. GET /v1/lanes/myapp-dev/guide with authorized token
        let resp = send(
            app.clone(),
            "GET",
            "/v1/lanes/myapp-dev/guide",
            Some(dev_token),
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let payload: Value =
            serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(payload["lane_id"], "myapp-dev");
        assert_eq!(payload["project"], "myapp");
        assert_eq!(payload["environment"], "dev");

        let sections = payload["sections"].as_array().unwrap();
        assert_eq!(sections.len(), 4);
        // Ordering: project -> environment -> component in lane.components declaration order
        assert_eq!(sections[0]["scope"], "project");
        assert_eq!(sections[0]["name"], "project");
        assert_eq!(
            sections[0]["content"],
            "# Project Overview\nGeneral guidelines."
        );
        assert_eq!(
            sections[0]["content_sha256"],
            hex::encode(Sha256::digest(b"# Project Overview\nGeneral guidelines."))
        );

        assert_eq!(sections[1]["scope"], "environment");
        assert_eq!(sections[1]["name"], "dev");
        assert_eq!(
            sections[1]["content"],
            "# Dev Environment\nLocal dev guidelines."
        );

        // In myapp-dev, components was declared as ["worker", "app"]
        assert_eq!(sections[2]["scope"], "component");
        assert_eq!(sections[2]["name"], "worker");
        assert_eq!(
            sections[2]["content"],
            "# Worker Component\nWorker guidelines."
        );

        assert_eq!(sections[3]["scope"], "component");
        assert_eq!(sections[3]["name"], "app");
        assert_eq!(sections[3]["content"], "# App Component\nApp guidelines.");

        // 2. Unconfigured lane returns 200 with empty sections
        let resp_unconfigured = send(
            app.clone(),
            "GET",
            "/v1/lanes/other-lane/guide",
            Some(other_token),
            None,
        )
        .await;
        assert_eq!(resp_unconfigured.status(), StatusCode::OK);
        let payload_unconf: Value = serde_json::from_slice(
            &resp_unconfigured
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes(),
        )
        .unwrap();
        assert_eq!(payload_unconf["sections"].as_array().unwrap().len(), 0);

        // 3. Unauthorized token for lane guide returns 403
        let resp_forbidden = send(
            app.clone(),
            "GET",
            "/v1/lanes/myapp-dev/guide",
            Some(other_token),
            None,
        )
        .await;
        assert_eq!(resp_forbidden.status(), StatusCode::FORBIDDEN);

        // 4. Non-existent lane returns 404
        let resp_missing = send(
            app.clone(),
            "GET",
            "/v1/lanes/nonexistent/guide",
            Some(dev_token),
            None,
        )
        .await;
        assert_eq!(resp_missing.status(), StatusCode::NOT_FOUND);

        // 5. GET /v1/guides/myapp aggregates visible lanes and deduplicates by file path
        let resp_project = send(
            app.clone(),
            "GET",
            "/v1/guides/myapp",
            Some(dev_token),
            None,
        )
        .await;
        assert_eq!(resp_project.status(), StatusCode::OK);
        let proj_payload: Value =
            serde_json::from_slice(&resp_project.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(proj_payload["project"], "myapp");
        assert_eq!(
            proj_payload["overview"]["content"],
            "# Project Overview\nGeneral guidelines."
        );

        let envs = proj_payload["environments"].as_array().unwrap();
        assert_eq!(
            envs.len(),
            1,
            "only dev environment is visible to dev-operator"
        );
        assert_eq!(envs[0]["environment"], "dev");
        let env_lanes = envs[0]["lane_ids"].as_array().unwrap();
        assert_eq!(env_lanes.len(), 2);
        assert_eq!(env_lanes[0], "myapp-dev");
        assert_eq!(env_lanes[1], "myapp-dev-canary");

        let comps = proj_payload["components"].as_array().unwrap();
        assert_eq!(comps.len(), 2);
        assert_eq!(comps[0]["component"], "app");
        assert_eq!(comps[1]["component"], "worker");

        // 6. Token with no visible lane in project returns 403
        let resp_proj_forbidden = send(
            app.clone(),
            "GET",
            "/v1/guides/myapp",
            Some(other_token),
            None,
        )
        .await;
        assert_eq!(resp_proj_forbidden.status(), StatusCode::FORBIDDEN);
        let err_body: Value = serde_json::from_slice(
            &resp_proj_forbidden
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes(),
        )
        .unwrap();
        assert_eq!(err_body["code"], "forbidden");
        assert_eq!(err_body["retryable"], false);
    }

    #[tokio::test]
    async fn guide_startup_rejections_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let mirror = temp.path().join("source.git");
        let output = std::process::Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&mirror)
            .output()
            .unwrap();
        assert!(output.status.success());

        let token_env = format!("SHIPBRAID_STARTUP_TEST_{}", Uuid::new_v4().simple());
        let token = "test-operator-credential-32-bytes-long";
        unsafe { std::env::set_var(&token_env, token) };

        // Test 1: Secret pattern in guide file rejects Service::open
        let secret_file = temp.path().join("secret.md");
        std::fs::write(&secret_file, "key = AKIAIOSFODNN7EXAMPLE").unwrap();

        let make_config = |guide_file: PathBuf| Config {
            database_path: temp
                .path()
                .join(format!("state-{}.db", Uuid::new_v4().simple())),
            tokens: vec![TokenConfig {
                id: "operator".into(),
                token_env: token_env.clone(),
                lanes: vec!["lane".into()],
                projects: vec![],
                urgent: false,
                admin: true,
                executor: false,
            }],
            lanes: vec![LaneConfig {
                id: "lane".into(),
                project: "project".into(),
                environment: "dev".into(),
                repository_id: "repo".into(),
                repository_path: mirror.clone(),
                release_ref: "refs/heads/main".into(),
                profile_digest: "synthetic-profile".into(),
                components: vec!["app".into()],
                allow_descendant: false,
                allow_preemption: false,
                failure_observe: None,
                resource_admission: None,
                request_parameters: Default::default(),
                enabled: true,
                executor: LaneExecutorConfig::Scripted,
                guide: Some(crate::config::GuideConfig {
                    project: Some(guide_file),
                    environment: None,
                    components: BTreeMap::new(),
                }),
                credentials: None,
                hook_environment: None,
            }],
            ..Config::default()
        };

        assert!(Service::open(make_config(secret_file)).await.is_err());

        // Test 2: Oversized file rejects Service::open
        let large_file = temp.path().join("large.md");
        std::fs::write(
            &large_file,
            vec![b'x'; crate::config::MAX_GUIDE_FILE_BYTES + 1],
        )
        .unwrap();
        assert!(Service::open(make_config(large_file)).await.is_err());

        // Test 3: Component not in lane.components rejects Service::open
        let valid_file = temp.path().join("valid.md");
        std::fs::write(&valid_file, "# Valid Guide").unwrap();
        let mut bad_component_config = make_config(valid_file.clone());
        bad_component_config.lanes[0].guide = Some(crate::config::GuideConfig {
            project: None,
            environment: None,
            components: BTreeMap::from([("nonexistent-comp".into(), valid_file)]),
        });
        assert!(Service::open(bad_component_config).await.is_err());

        unsafe { std::env::remove_var(&token_env) };
    }

    #[tokio::test]
    async fn lane_guide_endpoint_order_and_selective_sections() {
        let temp = tempfile::tempdir().unwrap();
        let mirror = temp.path().join("source.git");
        let output = std::process::Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&mirror)
            .output()
            .unwrap();
        assert!(output.status.success());

        let guides_dir = temp.path().join("guides");
        std::fs::create_dir_all(&guides_dir).unwrap();
        let proj_file = guides_dir.join("proj.md");
        std::fs::write(&proj_file, "# Project Title").unwrap();
        let env_file = guides_dir.join("env.md");
        std::fs::write(&env_file, "# Env Dev").unwrap();
        let zeta_file = guides_dir.join("zeta.md");
        std::fs::write(&zeta_file, "# Zeta Component").unwrap();
        let alpha_file = guides_dir.join("alpha.md");
        std::fs::write(&alpha_file, "# Alpha Component").unwrap();
        let beta_file = guides_dir.join("beta.md");
        std::fs::write(&beta_file, "# Beta Component").unwrap();

        let token_env = format!("SHIPBRAID_ORDER_TEST_{}", Uuid::new_v4().simple());
        let token = "order-test-token-credential-32-chars";
        unsafe { std::env::set_var(&token_env, token) };

        let config = Config {
            database_path: temp.path().join("state.db"),
            tokens: vec![TokenConfig {
                id: "operator".into(),
                token_env: token_env.clone(),
                lanes: vec![
                    "order-lane".into(),
                    "partial-env-comp".into(),
                    "partial-proj-only".into(),
                ],
                projects: vec![],
                urgent: false,
                admin: false,
                executor: false,
            }],
            lanes: vec![
                // 1. Full guide with reverse component declaration order: zeta, alpha, beta
                LaneConfig {
                    id: "order-lane".into(),
                    project: "proj".into(),
                    environment: "dev".into(),
                    repository_id: "repo".into(),
                    repository_path: mirror.clone(),
                    release_ref: "refs/heads/main".into(),
                    profile_digest: "synthetic-profile".into(),
                    components: vec!["zeta".into(), "alpha".into(), "beta".into()],
                    allow_descendant: false,
                    allow_preemption: false,
                    failure_observe: None,
                    resource_admission: None,
                    request_parameters: Default::default(),
                    enabled: true,
                    executor: LaneExecutorConfig::Scripted,
                    guide: Some(crate::config::GuideConfig {
                        project: Some(proj_file.clone()),
                        environment: Some(env_file.clone()),
                        components: BTreeMap::from([
                            ("alpha".into(), alpha_file.clone()),
                            ("beta".into(), beta_file.clone()),
                            ("zeta".into(), zeta_file.clone()),
                        ]),
                    }),
                    credentials: None,
                    hook_environment: None,
                },
                // 2. Partial guide: only environment and beta
                LaneConfig {
                    id: "partial-env-comp".into(),
                    project: "proj".into(),
                    environment: "staging".into(),
                    repository_id: "repo".into(),
                    repository_path: mirror.clone(),
                    release_ref: "refs/heads/main".into(),
                    profile_digest: "synthetic-profile".into(),
                    components: vec!["beta".into()],
                    allow_descendant: false,
                    allow_preemption: false,
                    failure_observe: None,
                    resource_admission: None,
                    request_parameters: Default::default(),
                    enabled: true,
                    executor: LaneExecutorConfig::Scripted,
                    guide: Some(crate::config::GuideConfig {
                        project: None,
                        environment: Some(env_file.clone()),
                        components: BTreeMap::from([("beta".into(), beta_file.clone())]),
                    }),
                    credentials: None,
                    hook_environment: None,
                },
                // 3. Partial guide: only project
                LaneConfig {
                    id: "partial-proj-only".into(),
                    project: "proj".into(),
                    environment: "prod".into(),
                    repository_id: "repo".into(),
                    repository_path: mirror.clone(),
                    release_ref: "refs/heads/main".into(),
                    profile_digest: "synthetic-profile".into(),
                    components: vec!["alpha".into()],
                    allow_descendant: false,
                    allow_preemption: false,
                    failure_observe: None,
                    resource_admission: None,
                    request_parameters: Default::default(),
                    enabled: true,
                    executor: LaneExecutorConfig::Scripted,
                    guide: Some(crate::config::GuideConfig {
                        project: Some(proj_file.clone()),
                        environment: None,
                        components: BTreeMap::new(),
                    }),
                    credentials: None,
                    hook_environment: None,
                },
            ],
            ..Config::default()
        };

        let service = Service::open(config).await.unwrap();
        unsafe { std::env::remove_var(&token_env) };
        let app = router(service);

        // Assert order in full guide: project -> environment -> component in lane declaration order
        let resp = send(
            app.clone(),
            "GET",
            "/v1/lanes/order-lane/guide",
            Some(token),
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let payload: Value =
            serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
        let sections = payload["sections"].as_array().unwrap();
        assert_eq!(sections.len(), 5);

        assert_eq!(sections[0]["scope"], "project");
        assert_eq!(sections[0]["name"], "project");

        assert_eq!(sections[1]["scope"], "environment");
        assert_eq!(sections[1]["name"], "dev");

        // Components MUST follow lane.components declaration order ["zeta", "alpha", "beta"],
        // not the BTreeMap key order ["alpha", "beta", "zeta"]
        assert_eq!(sections[2]["scope"], "component");
        assert_eq!(sections[2]["name"], "zeta");

        assert_eq!(sections[3]["scope"], "component");
        assert_eq!(sections[3]["name"], "alpha");

        assert_eq!(sections[4]["scope"], "component");
        assert_eq!(sections[4]["name"], "beta");

        // Assert partial guide 1: only environment and beta
        let resp_partial = send(
            app.clone(),
            "GET",
            "/v1/lanes/partial-env-comp/guide",
            Some(token),
            None,
        )
        .await;
        assert_eq!(resp_partial.status(), StatusCode::OK);
        let partial_payload: Value =
            serde_json::from_slice(&resp_partial.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let p_sections = partial_payload["sections"].as_array().unwrap();
        assert_eq!(p_sections.len(), 2);
        assert_eq!(p_sections[0]["scope"], "environment");
        assert_eq!(p_sections[0]["name"], "staging");
        assert_eq!(p_sections[1]["scope"], "component");
        assert_eq!(p_sections[1]["name"], "beta");

        // Assert partial guide 2: only project
        let resp_proj_only = send(
            app.clone(),
            "GET",
            "/v1/lanes/partial-proj-only/guide",
            Some(token),
            None,
        )
        .await;
        assert_eq!(resp_proj_only.status(), StatusCode::OK);
        let po_payload: Value = serde_json::from_slice(
            &resp_proj_only
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes(),
        )
        .unwrap();
        let po_sections = po_payload["sections"].as_array().unwrap();
        assert_eq!(po_sections.len(), 1);
        assert_eq!(po_sections[0]["scope"], "project");
        assert_eq!(po_sections[0]["name"], "project");
    }

    #[tokio::test]
    async fn lane_guide_and_project_guide_authorization_and_executor_rejection() {
        let temp = tempfile::tempdir().unwrap();
        let mirror = temp.path().join("source.git");
        let output = std::process::Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&mirror)
            .output()
            .unwrap();
        assert!(output.status.success());

        let guides_dir = temp.path().join("guides");
        std::fs::create_dir_all(&guides_dir).unwrap();
        let proj_file = guides_dir.join("proj.md");
        std::fs::write(&proj_file, "# Project Guide").unwrap();

        let admin_token_env = format!("SHIPBRAID_AUTH_ADMIN_{}", Uuid::new_v4().simple());
        let admin_token = "admin-credential-32-bytes-long00";
        unsafe { std::env::set_var(&admin_token_env, admin_token) };

        let proj_token_env = format!("SHIPBRAID_AUTH_PROJ_{}", Uuid::new_v4().simple());
        let proj_token = "proj-credential-32-bytes-long000";
        unsafe { std::env::set_var(&proj_token_env, proj_token) };

        let lane_token_env = format!("SHIPBRAID_AUTH_LANE_{}", Uuid::new_v4().simple());
        let lane_token = "lane-credential-32-bytes-long000";
        unsafe { std::env::set_var(&lane_token_env, lane_token) };

        let exec_token_env = format!("SHIPBRAID_AUTH_EXEC_{}", Uuid::new_v4().simple());
        let exec_token = "exec-credential-32-bytes-long000";
        unsafe { std::env::set_var(&exec_token_env, exec_token) };

        let other_token_env = format!("SHIPBRAID_AUTH_OTHER_{}", Uuid::new_v4().simple());
        let other_token = "other-credential-32-bytes-long00";
        unsafe { std::env::set_var(&other_token_env, other_token) };

        let config = Config {
            database_path: temp.path().join("state.db"),
            tokens: vec![
                TokenConfig {
                    id: "admin-token".into(),
                    token_env: admin_token_env.clone(),
                    lanes: vec![],
                    projects: vec![],
                    urgent: false,
                    admin: true,
                    executor: false,
                },
                TokenConfig {
                    id: "proj-token".into(),
                    token_env: proj_token_env.clone(),
                    lanes: vec![],
                    projects: vec!["myapp".into()],
                    urgent: false,
                    admin: false,
                    executor: false,
                },
                TokenConfig {
                    id: "lane-token".into(),
                    token_env: lane_token_env.clone(),
                    lanes: vec!["myapp-dev".into()],
                    projects: vec![],
                    urgent: false,
                    admin: false,
                    executor: false,
                },
                TokenConfig {
                    id: "exec-token".into(),
                    token_env: exec_token_env.clone(),
                    lanes: vec!["myapp-dev".into()],
                    projects: vec!["myapp".into()],
                    urgent: false,
                    admin: false,
                    executor: true,
                },
                TokenConfig {
                    id: "other-token".into(),
                    token_env: other_token_env.clone(),
                    lanes: vec!["other-lane".into()],
                    projects: vec![],
                    urgent: false,
                    admin: false,
                    executor: false,
                },
            ],
            lanes: vec![
                LaneConfig {
                    id: "myapp-dev".into(),
                    project: "myapp".into(),
                    environment: "dev".into(),
                    repository_id: "repo".into(),
                    repository_path: mirror.clone(),
                    release_ref: "refs/heads/main".into(),
                    profile_digest: "synthetic-profile".into(),
                    components: vec!["app".into()],
                    allow_descendant: false,
                    allow_preemption: false,
                    failure_observe: None,
                    resource_admission: None,
                    request_parameters: Default::default(),
                    enabled: true,
                    executor: LaneExecutorConfig::Scripted,
                    guide: Some(crate::config::GuideConfig {
                        project: Some(proj_file.clone()),
                        environment: None,
                        components: BTreeMap::new(),
                    }),
                    credentials: None,
                    hook_environment: None,
                },
                LaneConfig {
                    id: "other-lane".into(),
                    project: "other-app".into(),
                    environment: "test".into(),
                    repository_id: "repo".into(),
                    repository_path: mirror.clone(),
                    release_ref: "refs/heads/main".into(),
                    profile_digest: "synthetic-profile".into(),
                    components: vec!["app".into()],
                    allow_descendant: false,
                    allow_preemption: false,
                    failure_observe: None,
                    resource_admission: None,
                    request_parameters: Default::default(),
                    enabled: true,
                    executor: LaneExecutorConfig::Scripted,
                    guide: None,
                    credentials: None,
                    hook_environment: None,
                },
            ],
            ..Config::default()
        };

        let service = Service::open(config).await.unwrap();
        unsafe {
            std::env::remove_var(&admin_token_env);
            std::env::remove_var(&proj_token_env);
            std::env::remove_var(&lane_token_env);
            std::env::remove_var(&exec_token_env);
            std::env::remove_var(&other_token_env);
        }
        let app = router(service);

        // 1. GET /v1/lanes/myapp-dev/guide permissions
        // Admin token -> OK
        let r = send(
            app.clone(),
            "GET",
            "/v1/lanes/myapp-dev/guide",
            Some(admin_token),
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);

        // Project-scoped token -> OK
        let r = send(
            app.clone(),
            "GET",
            "/v1/lanes/myapp-dev/guide",
            Some(proj_token),
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);

        // Lane-scoped token -> OK
        let r = send(
            app.clone(),
            "GET",
            "/v1/lanes/myapp-dev/guide",
            Some(lane_token),
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);

        // Executor token -> FORBIDDEN (require_non_executor)
        let r = send(
            app.clone(),
            "GET",
            "/v1/lanes/myapp-dev/guide",
            Some(exec_token),
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // Unauthorized token -> FORBIDDEN
        let r = send(
            app.clone(),
            "GET",
            "/v1/lanes/myapp-dev/guide",
            Some(other_token),
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // 2. GET /v1/guides/myapp permissions
        // Admin token -> OK
        let r = send(
            app.clone(),
            "GET",
            "/v1/guides/myapp",
            Some(admin_token),
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);

        // Project-scoped token -> OK
        let r = send(
            app.clone(),
            "GET",
            "/v1/guides/myapp",
            Some(proj_token),
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);

        // Lane-scoped token -> OK
        let r = send(
            app.clone(),
            "GET",
            "/v1/guides/myapp",
            Some(lane_token),
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);

        // Executor token -> FORBIDDEN (require_non_executor)
        let r = send(
            app.clone(),
            "GET",
            "/v1/guides/myapp",
            Some(exec_token),
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // Other token (no visible lanes in myapp) -> FORBIDDEN
        let r = send(
            app.clone(),
            "GET",
            "/v1/guides/myapp",
            Some(other_token),
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn project_guide_cross_environment_aggregation_and_null_overview() {
        let temp = tempfile::tempdir().unwrap();
        let mirror = temp.path().join("source.git");
        let output = std::process::Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&mirror)
            .output()
            .unwrap();
        assert!(output.status.success());

        let guides_dir = temp.path().join("guides");
        std::fs::create_dir_all(&guides_dir).unwrap();
        let dev_env_file = guides_dir.join("dev.md");
        std::fs::write(&dev_env_file, "# Dev Guidelines").unwrap();
        let staging_env_file = guides_dir.join("staging.md");
        std::fs::write(&staging_env_file, "# Staging Guidelines").unwrap();

        let web_comp_file = guides_dir.join("web.md");
        std::fs::write(&web_comp_file, "# Web Component").unwrap();
        let api_comp_file = guides_dir.join("api.md");
        std::fs::write(&api_comp_file, "# API Component").unwrap();
        let worker_comp_file = guides_dir.join("worker.md");
        std::fs::write(&worker_comp_file, "# Worker Component").unwrap();

        let token_env = format!("SHIPBRAID_AGG_TEST_{}", Uuid::new_v4().simple());
        let token = "agg-operator-token-32-bytes-long";
        unsafe { std::env::set_var(&token_env, token) };

        let config = Config {
            database_path: temp.path().join("state.db"),
            tokens: vec![TokenConfig {
                id: "agg-operator".into(),
                token_env: token_env.clone(),
                lanes: vec!["agg-dev-1".into(), "agg-dev-2".into(), "agg-staging".into()],
                projects: vec![],
                urgent: false,
                admin: false,
                executor: false,
            }],
            lanes: vec![
                // Lane 1: dev env, web & api, NO project overview
                LaneConfig {
                    id: "agg-dev-1".into(),
                    project: "agg-app".into(),
                    environment: "dev".into(),
                    repository_id: "repo".into(),
                    repository_path: mirror.clone(),
                    release_ref: "refs/heads/main".into(),
                    profile_digest: "synthetic-profile".into(),
                    components: vec!["web".into(), "api".into()],
                    allow_descendant: false,
                    allow_preemption: false,
                    failure_observe: None,
                    resource_admission: None,
                    request_parameters: Default::default(),
                    enabled: true,
                    executor: LaneExecutorConfig::Scripted,
                    guide: Some(crate::config::GuideConfig {
                        project: None,
                        environment: Some(dev_env_file.clone()),
                        components: BTreeMap::from([
                            ("web".into(), web_comp_file.clone()),
                            ("api".into(), api_comp_file.clone()),
                        ]),
                    }),
                    credentials: None,
                    hook_environment: None,
                },
                // Lane 2: dev env, web only, sharing dev_env_file and web_comp_file, NO project overview
                LaneConfig {
                    id: "agg-dev-2".into(),
                    project: "agg-app".into(),
                    environment: "dev".into(),
                    repository_id: "repo".into(),
                    repository_path: mirror.clone(),
                    release_ref: "refs/heads/main".into(),
                    profile_digest: "synthetic-profile".into(),
                    components: vec!["web".into()],
                    allow_descendant: false,
                    allow_preemption: false,
                    failure_observe: None,
                    resource_admission: None,
                    request_parameters: Default::default(),
                    enabled: true,
                    executor: LaneExecutorConfig::Scripted,
                    guide: Some(crate::config::GuideConfig {
                        project: None,
                        environment: Some(dev_env_file.clone()),
                        components: BTreeMap::from([("web".into(), web_comp_file.clone())]),
                    }),
                    credentials: None,
                    hook_environment: None,
                },
                // Lane 3: staging env, api & worker, sharing api_comp_file with dev, NO project overview
                LaneConfig {
                    id: "agg-staging".into(),
                    project: "agg-app".into(),
                    environment: "staging".into(),
                    repository_id: "repo".into(),
                    repository_path: mirror.clone(),
                    release_ref: "refs/heads/main".into(),
                    profile_digest: "synthetic-profile".into(),
                    components: vec!["api".into(), "worker".into()],
                    allow_descendant: false,
                    allow_preemption: false,
                    failure_observe: None,
                    resource_admission: None,
                    request_parameters: Default::default(),
                    enabled: true,
                    executor: LaneExecutorConfig::Scripted,
                    guide: Some(crate::config::GuideConfig {
                        project: None,
                        environment: Some(staging_env_file.clone()),
                        components: BTreeMap::from([
                            ("api".into(), api_comp_file.clone()),
                            ("worker".into(), worker_comp_file.clone()),
                        ]),
                    }),
                    credentials: None,
                    hook_environment: None,
                },
            ],
            ..Config::default()
        };

        let service = Service::open(config).await.unwrap();
        unsafe { std::env::remove_var(&token_env) };
        let app = router(service);

        let resp = send(app.clone(), "GET", "/v1/guides/agg-app", Some(token), None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let payload: Value =
            serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();

        assert_eq!(payload["project"], "agg-app");
        // Overview is null when unconfigured
        assert!(
            payload["overview"].is_null(),
            "expected overview to be null when unconfigured"
        );

        // Environments aggregated across dev and staging
        let envs = payload["environments"].as_array().unwrap();
        assert_eq!(envs.len(), 2);

        // Ordered alphabetically: dev, then staging
        assert_eq!(envs[0]["environment"], "dev");
        let dev_lanes = envs[0]["lane_ids"].as_array().unwrap();
        assert_eq!(dev_lanes.len(), 2);
        assert_eq!(dev_lanes[0], "agg-dev-1");
        assert_eq!(dev_lanes[1], "agg-dev-2");

        assert_eq!(envs[1]["environment"], "staging");
        let staging_lanes = envs[1]["lane_ids"].as_array().unwrap();
        assert_eq!(staging_lanes.len(), 1);
        assert_eq!(staging_lanes[0], "agg-staging");

        // Components deduplicated by file path: "web", "api", "worker"
        let comps = payload["components"].as_array().unwrap();
        assert_eq!(
            comps.len(),
            3,
            "components must be deduplicated across lanes"
        );
        // Ordered alphabetically: api, web, worker
        assert_eq!(comps[0]["component"], "api");
        assert_eq!(comps[1]["component"], "web");
        assert_eq!(comps[2]["component"], "worker");
    }
}
