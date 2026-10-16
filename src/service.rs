use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, bail};
use chrono::{Duration as ChronoDuration, Utc};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::{
    api::{self, ExecutorEventKind, ResolveAction},
    config::{Config, HookConfig, LaneExecutorConfig, ResolvedToken},
    domain::{
        Command, CommandDelivery, CommandDeliveryState, CommandKind, CoverageMode, DomainError,
        ExecutorResult, LOCAL_TENANT_ID, Lane, Phase, Priority, ReconcileIds, ReleaseRequest,
        RequestContext, RequestStatus, RunState, State, SubmitInput, SubmitOutcome, Target,
        UnknownResolution,
    },
    engine::{Engine, EngineConfig},
    executor::{Executor, ScriptedExecutor},
    hooks::{HookCall, HookEnvelope, HookExecutor, HookOperation, HookReply, HookStatus},
    lineage::{CoverageContext, CoverageQuery, LineageResolver, is_full_revision},
    store::Store,
};

const MAX_PENDING_REQUESTS_PER_LANE: usize = 1_000;
const MAX_MANUAL_RETRIES: u32 = 3;

#[derive(Clone)]
pub(crate) struct RequestActor {
    pub id: String,
    pub lanes: Vec<String>,
    pub projects: Vec<String>,
    pub urgent: bool,
    pub admin: bool,
    pub executor: bool,
}

impl RequestActor {
    pub fn lane_allowed(&self, lane: &Lane) -> bool {
        self.admin || self.lanes.contains(&lane.id) || self.projects.contains(&lane.project)
    }

    fn operation_allowed(&self, request: &ReleaseRequest, state: &State, urgent: bool) -> bool {
        self.request_allowed(request, state) && (!urgent || self.urgent || self.admin)
    }

    fn request_allowed(&self, request: &ReleaseRequest, state: &State) -> bool {
        (!self.executor || self.admin)
            && state
                .lanes
                .get(&request.lane_id)
                .is_some_and(|lane| self.lane_allowed(lane))
            && (self.admin
                || request.principal == self.id
                || request.context.collaborators.contains(&self.id))
    }
}

pub(crate) fn request_allowed_actions(
    actor: &RequestActor,
    request: &ReleaseRequest,
    state: &State,
    now: chrono::DateTime<Utc>,
) -> Vec<&'static str> {
    if !actor.request_allowed(request, state) {
        return Vec::new();
    }
    let mut actions = Vec::new();
    if !request.status.is_terminal() {
        actions.push("withdraw");
        if request.priority == Priority::Normal && actor.operation_allowed(request, state, true) {
            actions.push("raise_priority");
        }
    }
    if request.status == RequestStatus::NeedsAttention
        && request.deadline_at.is_none_or(|deadline| deadline > now)
        && request.manual_retries_used < MAX_MANUAL_RETRIES
        && actor.operation_allowed(request, state, request.priority == Priority::Urgent)
    {
        actions.push("retry");
    }
    actions
}

pub(crate) enum RequestMutation {
    Withdraw,
    Retry,
    Priority(Priority, String),
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("the authenticated actor is not authorized for this operation")]
    Forbidden,
    #[error("invalid request context: {0}")]
    InvalidRequestContext(String),
    #[error(transparent)]
    Domain(#[from] DomainError),
    #[error("persistent storage operation failed: {0}")]
    Storage(#[source] anyhow::Error),
    #[error("the lane has reached its pending request limit")]
    QueueFull,
    #[error("the coordinator is not ready")]
    NotReady,
    #[error("the requested operation cannot be proven from the scripted executor record")]
    UnprovenExecutorState,
}

impl From<anyhow::Error> for ServiceError {
    fn from(value: anyhow::Error) -> Self {
        Self::Storage(value)
    }
}

pub struct Service {
    config: Config,
    tokens: Vec<ResolvedToken>,
    store: Store,
    state: RwLock<State>,
    writer: Mutex<()>,
    executor_gate: Mutex<()>,
    engine: Engine,
    lineage: LineageResolver,
    executor: ScriptedExecutor,
    lane_executors: BTreeMap<String, LaneExecutorConfig>,
    _database_lock: DatabaseLock,
    ready: AtomicBool,
}

impl Service {
    pub async fn open(config: Config) -> anyhow::Result<Arc<Self>> {
        config
            .validate()
            .context("validate coordinator configuration")?;
        let tokens = config.resolve_tokens().context("resolve API credentials")?;
        let database_path = canonical_database_path(&config.database_path)?;
        let database_lock = acquire_database_lock(&database_path)?;

        let mut mirrors = BTreeMap::new();
        for lane in &config.lanes {
            let path = lane.repository_path.clone();
            if let Some(previous) = mirrors.insert(lane.repository_id.clone(), path.clone()) {
                let previous = previous
                    .canonicalize()
                    .with_context(|| format!("resolve repository mirror {}", previous.display()))?;
                let current = path
                    .canonicalize()
                    .with_context(|| format!("resolve repository mirror {}", path.display()))?;
                if previous != current {
                    bail!(
                        "repository id {} maps to more than one local mirror",
                        lane.repository_id
                    );
                }
            }
        }
        let lineage = LineageResolver::new(mirrors).context("validate local bare Git mirrors")?;
        let store = Store::open(&database_path).await?;
        let mut state = store.load_state().await?;
        let before_config = state.clone();
        let engine = Engine::new(EngineConfig {
            max_active_runs: config.max_active_runs,
            coalesce_window: ChronoDuration::milliseconds(
                i64::try_from(config.coalesce_ms).unwrap_or(i64::MAX),
            ),
            max_manual_retries: MAX_MANUAL_RETRIES,
        });
        let now = Utc::now();
        for lane_config in &config.lanes {
            let release_line_id = release_line_id(lane_config);
            let effective_profile_digest = config.effective_profile_digest(lane_config)?;
            let current = state.lanes.get(&lane_config.id);
            let mut lane = Lane {
                tenant_id: LOCAL_TENANT_ID.to_owned(),
                id: lane_config.id.clone(),
                project: lane_config.project.clone(),
                environment: lane_config.environment.clone(),
                enabled: lane_config.enabled,
                repository_id: lane_config.repository_id.clone(),
                release_ref: lane_config.release_ref.clone(),
                release_line_id,
                profile_digest: effective_profile_digest,
                config_version: current.map_or(1, |lane| lane.config_version),
                descendant_coverage_enabled: lane_config.allow_descendant,
                preemption_enabled: lane_config.allow_preemption
                    && matches!(&lane_config.executor, LaneExecutorConfig::Scripted),
                allowed_revisions: BTreeSet::new(),
                allowed_components: lane_config.components.iter().cloned().collect(),
                active_run_id: None,
                generation: 0,
                blocked_reason: None,
                failure_paused: false,
            };
            if let Some(existing) = current
                && !lane_config_matches(existing, &lane)
            {
                lane.config_version = existing.config_version.saturating_add(1);
            }
            engine.register_lane(&mut state, lane, now)?;
        }
        let configured_lanes = config
            .lanes
            .iter()
            .map(|lane| lane.id.as_str())
            .collect::<BTreeSet<_>>();
        let removed_lanes = state
            .lanes
            .values()
            .filter(|lane| !configured_lanes.contains(lane.id.as_str()) && lane.enabled)
            .cloned()
            .collect::<Vec<_>>();
        for mut lane in removed_lanes {
            lane.enabled = false;
            lane.config_version = lane.config_version.saturating_add(1);
            engine.register_lane(&mut state, lane, now)?;
        }
        if state != before_config {
            store.commit_transition(&before_config, &state).await?;
        }

        // A delivered command without its result has an unknown outcome. Keep
        // its lane fenced and require evidence; a pending command has not yet
        // crossed the executor boundary and may be delivered once.
        let active_runs = state
            .runs
            .values()
            .filter(|run| run.state.owns_execution_slot())
            .cloned()
            .collect::<Vec<_>>();
        let mut recovered = state.clone();
        for run in active_runs {
            if run.state == RunState::Unknown {
                continue;
            }
            let lane_profile_changed = recovered.lanes.get(&run.lane_id).is_none_or(|lane| {
                lane.profile_digest != run.target.profile_digest
                    || lane.repository_id != run.target.repository_id
                    || lane.release_line_id != run.target.release_line_id
            });
            let executor_unconfigured = !config.lanes.iter().any(|lane| lane.id == run.lane_id);
            let unresolved_delivery = recovered.commands.values().any(|command| {
                command.run_id == run.id
                    && command.generation == run.generation
                    && matches!(command.kind, CommandKind::Dispatch | CommandKind::Cancel)
                    && matches!(
                        command.delivery_state,
                        CommandDeliveryState::Delivered | CommandDeliveryState::Unknown
                    )
            });
            if lane_profile_changed || executor_unconfigured || unresolved_delivery {
                engine.mark_unknown(
                    &mut recovered,
                    run.id,
                    run.generation,
                    if lane_profile_changed || executor_unconfigured {
                        "active run does not match the configured execution profile; restore its original configuration before observing"
                    } else {
                        "service restarted after command delivery without a durable operation result"
                    },
                    now,
                )?;
            }
        }
        if recovered != state {
            store.commit_transition(&state, &recovered).await?;
            state = recovered;
        }

        let service = Arc::new(Self {
            config: config.clone(),
            tokens,
            store,
            state: RwLock::new(state),
            writer: Mutex::new(()),
            executor_gate: Mutex::new(()),
            engine,
            lineage,
            executor: ScriptedExecutor::new(Duration::from_millis(config.scripted_phase_ms)),
            lane_executors: config
                .lanes
                .iter()
                .map(|lane| (lane.id.clone(), lane.executor.clone()))
                .collect(),
            _database_lock: database_lock,
            ready: AtomicBool::new(true),
        });
        Ok(service)
    }

    pub fn tokens(&self) -> &[ResolvedToken] {
        &self.tokens
    }

    pub fn executor_kind(&self, lane_id: &str) -> &'static str {
        self.lane_executors
            .get(lane_id)
            .map_or("unconfigured", LaneExecutorConfig::kind)
    }

    pub fn executor_mix(&self) -> String {
        let mut kinds = self
            .lane_executors
            .values()
            .map(LaneExecutorConfig::kind)
            .collect::<BTreeSet<_>>();
        match kinds.len() {
            0 => "scripted".to_owned(),
            1 => kinds.pop_first().unwrap_or("scripted").to_owned(),
            _ => "mixed".to_owned(),
        }
    }

    pub fn dashboard_dir(&self) -> Option<&Path> {
        self.config.dashboard_dir.as_deref()
    }

    fn lane_executor(&self, lane_id: &str) -> Option<&LaneExecutorConfig> {
        self.lane_executors.get(lane_id)
    }

    pub async fn snapshot(&self) -> State {
        self.state.read().await.clone()
    }

    pub async fn ready(&self) -> Result<(), ServiceError> {
        if !self.ready.load(Ordering::Acquire) {
            return Err(ServiceError::NotReady);
        }
        self.store.ping().await.map_err(ServiceError::Storage)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Authenticated actor and immutable HTTP target fields are passed independently"
    )]
    pub async fn submit(
        &self,
        principal: String,
        idempotency_key: String,
        request_hash: String,
        lane_id: String,
        revision: String,
        components: Vec<String>,
        coverage_mode: CoverageMode,
        priority: Priority,
        deadline_at: Option<chrono::DateTime<Utc>>,
        reason: String,
    ) -> Result<SubmitOutcome, ServiceError> {
        self.submit_with_context(
            principal,
            idempotency_key,
            request_hash,
            lane_id,
            revision,
            components,
            coverage_mode,
            priority,
            deadline_at,
            reason,
            RequestContext::default(),
        )
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Immutable authenticated submission fields"
    )]
    pub async fn submit_with_context(
        &self,
        principal: String,
        idempotency_key: String,
        request_hash: String,
        lane_id: String,
        revision: String,
        components: Vec<String>,
        coverage_mode: CoverageMode,
        priority: Priority,
        deadline_at: Option<chrono::DateTime<Utc>>,
        reason: String,
        context: RequestContext,
    ) -> Result<SubmitOutcome, ServiceError> {
        // Resolve Git ancestry before taking the serialized ledger writer.
        // Concurrent submissions can therefore do their read-only local Git
        // checks in parallel instead of holding up commits behind a child
        // process. Keep the async runtime responsive while Git runs.
        if !is_full_revision(&revision) {
            return Err(DomainError::TargetNotAllowed(
                "revision must be a full immutable Git object ID".to_owned(),
            )
            .into());
        }
        let preliminary_lane = self
            .state
            .read()
            .await
            .lanes
            .get(&lane_id)
            .cloned()
            .ok_or_else(|| DomainError::LaneNotFound(lane_id.clone()))?;
        let lineage = self.lineage.clone();
        let repository_id = preliminary_lane.repository_id.clone();
        let release_ref = preliminary_lane.release_ref.clone();
        let proof_revision = revision.clone();
        let release_line_id = preliminary_lane.release_line_id.clone();
        let config_version = preliminary_lane.config_version;
        let authorization = tokio::task::spawn_blocking(move || {
            lineage.resolve_submission(
                &repository_id,
                &release_ref,
                &proof_revision,
                &release_line_id,
                config_version,
            )
        })
        .await
        .map_err(|error| ServiceError::Storage(anyhow::Error::new(error)))?;

        let _writer = self.writer.lock().await;
        let previous = self.state.read().await.clone();
        let lane = previous
            .lanes
            .get(&lane_id)
            .cloned()
            .ok_or_else(|| DomainError::LaneNotFound(lane_id.clone()))?;
        self.validate_context(&context, &lane)?;
        // Config may have been reloaded while the proof was being computed.
        // A proof for an old lane snapshot must never authorize a new target.
        if preliminary_lane.id != lane.id
            || preliminary_lane.repository_id != lane.repository_id
            || preliminary_lane.release_ref != lane.release_ref
            || preliminary_lane.release_line_id != lane.release_line_id
            || preliminary_lane.profile_digest != lane.profile_digest
            || preliminary_lane.config_version != lane.config_version
            || authorization.repository_id != lane.repository_id
            || authorization.release_ref != lane.release_ref
            || authorization.revision != revision
            || authorization.release_line_id != lane.release_line_id
            || authorization.config_version != lane.config_version
            || authorization.relation != crate::domain::LineageRelation::Ancestor
        {
            return Err(DomainError::LineageUnproven.into());
        }
        let target = Target {
            repository_id: lane.repository_id.clone(),
            revision: revision.clone(),
            components: components.into_iter().collect(),
            profile_digest: lane.profile_digest.clone(),
            release_line_id: lane.release_line_id.clone(),
            config_version: lane.config_version,
            coverage_mode,
        };
        let is_replay = previous.requests.values().any(|request| {
            request.principal == principal
                && request.idempotency_key == idempotency_key
                && previous
                    .lanes
                    .get(&request.lane_id)
                    .is_some_and(|existing_lane| existing_lane.project == lane.project)
        });
        let pending_count = previous
            .requests
            .values()
            .filter(|request| {
                request.lane_id == lane_id
                    && matches!(
                        request.status,
                        RequestStatus::Accepted
                            | RequestStatus::Waiting
                            | RequestStatus::NeedsAttention
                    )
            })
            .count();
        if !is_replay && pending_count >= MAX_PENDING_REQUESTS_PER_LANE {
            return Err(ServiceError::QueueFull);
        }
        let coverage = self.coverage_context(&previous, Some(&target), Some(&lane_id));
        let mut next = previous.clone();
        let result = self.engine.submit(
            &mut next,
            SubmitInput {
                id: Uuid::new_v4(),
                principal: principal.clone(),
                context,
                idempotency_key,
                request_hash,
                lane_id,
                target,
                authorization,
                priority,
                deadline_at,
                reason,
            },
            Utc::now(),
            &coverage,
        )?;
        for event in &mut next.events[previous.events.len()..] {
            event.actor = Some(principal.clone());
        }
        self.commit_publish(&previous, next).await?;
        Ok(result)
    }

    fn validate_context(&self, context: &RequestContext, lane: &Lane) -> Result<(), ServiceError> {
        for value in [&context.task_id, &context.session_id]
            .into_iter()
            .flatten()
        {
            if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                return Err(ServiceError::InvalidRequestContext("task_id/session_id must be nonempty, at most 256 bytes, and contain no control characters".into()));
            }
        }
        if context.collaborators.len() > 16
            || context.collaborators.iter().any(|id| {
                !self.tokens.iter().any(|token| {
                    token.id == *id
                        && !token.executor
                        && (token.admin
                            || token.lanes.contains(&lane.id)
                            || token.projects.contains(&lane.project))
                })
            })
        {
            return Err(ServiceError::InvalidRequestContext("collaborators must contain at most 16 configured non-executor principals authorized for this lane".into()));
        }
        Ok(())
    }

    pub(crate) async fn mutate_request(
        &self,
        request_id: Uuid,
        actor: &RequestActor,
        mutation: RequestMutation,
    ) -> Result<(), ServiceError> {
        self.transact(None, |engine, state, _| {
            let request = state
                .requests
                .get(&request_id)
                .cloned()
                .ok_or(DomainError::RequestNotFound)?;
            let requires_urgent =
                matches!(&mutation, RequestMutation::Priority(Priority::Urgent, _))
                    || (matches!(&mutation, RequestMutation::Retry)
                        && request.priority == Priority::Urgent);
            if !actor.operation_allowed(&request, state, requires_urgent) {
                return Err(ServiceError::Forbidden);
            }
            let start = state.events.len();
            let now = Utc::now();
            match mutation {
                // Engine requires the owner; only map after authenticating the actual actor.
                RequestMutation::Withdraw => {
                    engine.withdraw(state, request_id, &request.principal, now)?
                }
                RequestMutation::Retry => engine.retry(state, request_id, now)?,
                RequestMutation::Priority(priority, reason) => {
                    engine.set_priority(state, request_id, priority, reason, now)?
                }
            }
            for event in &mut state.events[start..] {
                event.actor = Some(actor.id.clone());
            }
            Ok(())
        })
        .await
    }

    pub async fn withdraw(&self, request_id: Uuid, principal: String) -> Result<(), ServiceError> {
        self.transact(None, |engine, state, _| {
            engine.withdraw(state, request_id, &principal, Utc::now())?;
            Ok(())
        })
        .await
    }

    pub async fn retry(&self, request_id: Uuid) -> Result<(), ServiceError> {
        self.transact(None, |engine, state, _| {
            engine.retry(state, request_id, Utc::now())?;
            Ok(())
        })
        .await
    }

    pub async fn set_priority(
        &self,
        request_id: Uuid,
        priority: Priority,
        reason: String,
    ) -> Result<(), ServiceError> {
        self.transact(None, |engine, state, _| {
            engine.set_priority(state, request_id, priority, reason, Utc::now())?;
            Ok(())
        })
        .await
    }

    pub async fn phase_grant(
        &self,
        run_id: Uuid,
        generation: u64,
        next_phase: Phase,
        executor_ref: String,
    ) -> Result<(), ServiceError> {
        if self.executor_kind_for_run(run_id).await != Some("scripted") {
            return Err(DomainError::InvalidTransition.into());
        }
        self.transact(None, |engine, state, _| {
            let previous_phase = state.runs.get(&run_id).map(|run| run.phase.clone());
            engine.phase_grant(
                state,
                run_id,
                generation,
                &executor_ref,
                next_phase.clone(),
                Utc::now(),
            )?;
            if previous_phase.as_ref() != Some(&next_phase) {
                engine.record_executor_log(
                    state,
                    run_id,
                    "info",
                    &format!("ScriptedExecutor granted phase {next_phase:?}"),
                    Utc::now(),
                )?;
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn executor_event(
        &self,
        run_id: Uuid,
        generation: u64,
        event: ExecutorEventKind,
        executor_ref: Option<String>,
        reason: String,
        environment_known_unchanged: bool,
    ) -> Result<(), ServiceError> {
        self.transact(None, |engine, state, coverage| {
            let run = state
                .runs
                .get(&run_id)
                .cloned()
                .ok_or(DomainError::RunNotFound)?;
            if self.executor_kind(&run.lane_id) != "scripted" {
                return Err(DomainError::InvalidTransition.into());
            }
            let command = state
                .commands
                .values()
                .filter(|command| command.run_id == run_id && command.generation == generation)
                .max_by_key(|command| command.created_at)
                .cloned();
            let now = Utc::now();
            match event {
                ExecutorEventKind::Started => {
                    let executor_ref = executor_ref
                        .filter(|value| !value.trim().is_empty())
                        .ok_or(DomainError::ExecutorMismatch)?;
                    let command = state
                        .commands
                        .values()
                        .filter(|command| {
                            command.run_id == run_id
                                && command.generation == generation
                                && command.kind == CommandKind::Dispatch
                        })
                        .max_by_key(|command| command.created_at)
                        .cloned()
                        .ok_or(DomainError::CommandNotFound)?;
                    engine.command_delivery(
                        state,
                        command.id,
                        CommandDelivery::Acknowledged,
                        now,
                    )?;
                    engine.executor_result(
                        state,
                        run_id,
                        generation,
                        &executor_ref,
                        ExecutorResult::Started {
                            executor_ref: executor_ref.clone(),
                        },
                        now,
                        coverage,
                    )?;
                }
                ExecutorEventKind::Stopped => {
                    let executor_ref = executor_ref
                        .or(run.executor_ref.clone())
                        .filter(|value| !value.trim().is_empty())
                        .ok_or(DomainError::OwnershipUnproven)?;
                    if let Some(command) =
                        command.filter(|command| command.kind == CommandKind::Cancel)
                    {
                        engine.command_delivery(
                            state,
                            command.id,
                            CommandDelivery::Acknowledged,
                            now,
                        )?;
                    }
                    engine.confirm_stopped(
                        state,
                        run_id,
                        generation,
                        &executor_ref,
                        Uuid::new_v4(),
                        now,
                        coverage,
                    )?;
                }
                ExecutorEventKind::Failed => {
                    let executor_ref = executor_ref
                        .or(run.executor_ref.clone())
                        .filter(|value| !value.trim().is_empty())
                        .ok_or(DomainError::ExecutorMismatch)?;
                    if let Some(command) = command {
                        engine.command_delivery(
                            state,
                            command.id,
                            CommandDelivery::Acknowledged,
                            now,
                        )?;
                    }
                    engine.executor_result(
                        state,
                        run_id,
                        generation,
                        &executor_ref,
                        ExecutorResult::Failed {
                            message: bounded_text(&reason, 2048),
                            environment_known_unchanged,
                        },
                        now,
                        coverage,
                    )?;
                }
                ExecutorEventKind::Unknown => {
                    // Transport acknowledgement stays historical fact. A
                    // later loss of execution observability independently
                    // blocks the run; it cannot be discarded as an ack replay.
                    if let (Some(expected), Some(supplied)) =
                        (run.executor_ref.as_deref(), executor_ref.as_deref())
                        && expected != supplied
                    {
                        return Err(DomainError::ExecutorMismatch.into());
                    }
                    engine.mark_unknown(
                        state,
                        run_id,
                        generation,
                        &bounded_text(&reason, 2048),
                        now,
                    )?;
                }
            }
            engine.record_executor_log(
                state,
                run_id,
                "info",
                &format!(
                    "executor reported {:?}: {}",
                    event,
                    bounded_text(&reason, 512)
                ),
                now,
            )?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn resolve_unknown(
        &self,
        lane_id: String,
        action: ResolveAction,
        evidence_note: String,
        reason: String,
    ) -> Result<(), ServiceError> {
        let _executor = self.executor_gate.lock().await;
        let snapshot = self.snapshot().await;
        let lane = snapshot
            .lanes
            .get(&lane_id)
            .cloned()
            .ok_or_else(|| DomainError::LaneNotFound(lane_id.clone()))?;
        let run_id = lane.active_run_id.ok_or(DomainError::RunNotFound)?;
        let run = snapshot
            .runs
            .get(&run_id)
            .cloned()
            .ok_or(DomainError::RunNotFound)?;
        if run.state != RunState::Unknown {
            return Err(DomainError::InvalidTransition.into());
        }
        if self.executor_kind(&lane_id) == "unconfigured" {
            return Err(ServiceError::UnprovenExecutorState);
        }
        if self.executor_kind(&lane_id) == "hooks" {
            return self
                .resolve_unknown_hook(&snapshot, &lane, &run, action, evidence_note, reason)
                .await;
        }
        let observation = self.executor.observe(&snapshot, &run);
        let observation_digest = hex::encode(Sha256::digest(
            format!(
                "scripted-observation:{}:{}:{observation:?}",
                run.id, run.generation
            )
            .as_bytes(),
        ));
        let command_id = Uuid::new_v4();
        let now = Utc::now();
        let resolution = match action {
            ResolveAction::ConfirmStopped => match observation {
                ExecutorResult::Stopped { executor_ref } => UnknownResolution::KnownStopped {
                    executor_ref: (!executor_ref.is_empty()).then_some(executor_ref),
                    evidence_digest: observation_digest,
                },
                _ => return Err(ServiceError::UnprovenExecutorState),
            },
            ResolveAction::AdoptRunning => match observation {
                ExecutorResult::Started { executor_ref } => UnknownResolution::KnownRunning {
                    executor_ref,
                    evidence_digest: observation_digest,
                },
                _ => return Err(ServiceError::UnprovenExecutorState),
            },
            ResolveAction::RecordVerified => match self.executor.verify(&run, now) {
                ExecutorResult::VerifiedReceipt(receipt) => UnknownResolution::VerifiedReceipt {
                    receipt,
                    evidence_digest: observation_digest,
                },
                _ => return Err(ServiceError::UnprovenExecutorState),
            },
        };
        self.transact(Some(&lane_id), |engine, state, coverage| {
            engine.resolve_unknown(
                state,
                run_id,
                run.generation,
                resolution,
                command_id,
                now,
                coverage,
            )?;
            let note = bounded_text(&evidence_note, 1024);
            let reason = bounded_text(&reason, 512);
            engine.record_executor_log(
                state,
                run_id,
                "warn",
                &format!("admin resolution via ScriptedExecutor observation; reason={reason}; note={note}"),
                now,
            )?;
            Ok(())
        })
        .await
    }

    pub async fn set_draining(&self, draining: bool) -> Result<(), ServiceError> {
        // Match process_pending_commands' lock order. Waiting for the
        // executor boundary ensures a durable drain cannot race a dispatch
        // that has already crossed into the scripted executor.
        let _executor = self.executor_gate.lock().await;
        self.transact(None, |engine, state, _| {
            engine.set_draining(state, draining, Utc::now());
            Ok(())
        })
        .await
    }

    pub async fn scheduler_tick(&self) -> Result<(), ServiceError> {
        if !self.ready.load(Ordering::Acquire) {
            return Err(ServiceError::NotReady);
        }
        self.advance_running_phases().await?;
        self.reconcile_lanes().await?;
        self.poll_hook_runs().await?;
        self.process_pending_commands().await?;
        self.check_cancel_timeouts().await?;
        Ok(())
    }

    async fn transact<T>(
        &self,
        lane_id: Option<&str>,
        apply: impl FnOnce(&Engine, &mut State, &CoverageContext) -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        let _writer = self.writer.lock().await;
        let previous = self.state.read().await.clone();
        let coverage = self.coverage_context(&previous, None, lane_id);
        self.transact_locked(previous, coverage, apply).await
    }

    async fn transact_locked<T>(
        &self,
        previous: State,
        coverage: CoverageContext,
        apply: impl FnOnce(&Engine, &mut State, &CoverageContext) -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        let mut next = previous.clone();
        let result = apply(&self.engine, &mut next, &coverage)?;
        self.commit_publish(&previous, next).await?;
        Ok(result)
    }

    async fn commit_publish(&self, previous: &State, next: State) -> Result<(), ServiceError> {
        if next == *previous {
            return Ok(());
        }
        self.store
            .commit_transition(previous, &next)
            .await
            .map_err(ServiceError::Storage)?;
        *self.state.write().await = next;
        Ok(())
    }

    fn coverage_context(
        &self,
        state: &State,
        extra_target: Option<&Target>,
        lane_filter: Option<&str>,
    ) -> CoverageContext {
        let lane_ids = lane_filter
            .map(|lane| vec![lane.to_owned()])
            .unwrap_or_else(|| state.lanes.keys().cloned().collect());
        let mut queries = BTreeSet::<CoverageQuery>::new();
        for lane_id in &lane_ids {
            queries.extend(self.engine.coverage_queries(state, lane_id));
        }

        // A not-yet-inserted submission can attach to an active run, so add
        // its candidate/request relationships before asking Git for a batch.
        if let (Some(extra), Some(lane_id)) = (extra_target, lane_filter) {
            let mut candidates = state
                .requests
                .values()
                .filter(|request| request.lane_id == lane_id && !request.status.is_terminal())
                .map(|request| request.target.clone())
                .collect::<Vec<_>>();
            if let Some(run_id) = state.lanes.get(lane_id).and_then(|lane| lane.active_run_id)
                && let Some(run) = state.runs.get(&run_id)
            {
                candidates.push(run.target.clone());
            }
            let mut requested = state
                .requests
                .values()
                .filter(|request| {
                    request.lane_id == lane_id
                        && !request.status.is_terminal()
                        && request.target.coverage_mode == CoverageMode::Descendant
                })
                .map(|request| request.target.clone())
                .collect::<Vec<_>>();
            requested.push(extra.clone());
            candidates.push(extra.clone());
            for candidate in &candidates {
                for request in &requested {
                    if candidate.revision == request.revision
                        || candidate.repository_id != request.repository_id
                        || candidate.release_line_id != request.release_line_id
                        || candidate.config_version != request.config_version
                        || candidate.profile_digest != request.profile_digest
                        || !request.components.is_subset(&candidate.components)
                        || request.coverage_mode != CoverageMode::Descendant
                        || !is_full_revision(&candidate.revision)
                        || !is_full_revision(&request.revision)
                    {
                        continue;
                    }
                    queries.insert(CoverageQuery {
                        repository_id: candidate.repository_id.clone(),
                        candidate_revision: candidate.revision.clone(),
                        requested_revision: request.revision.clone(),
                        release_line_id: candidate.release_line_id.clone(),
                        config_version: candidate.config_version,
                    });
                }
            }
        }
        self.lineage
            .resolve_coverage_context(&queries.into_iter().collect::<Vec<_>>())
    }

    async fn reconcile_lanes(&self) -> Result<(), ServiceError> {
        let lane_ids = self
            .state
            .read()
            .await
            .lanes
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for lane_id in lane_ids {
            self.transact(Some(&lane_id), |engine, state, coverage| {
                engine.reconcile(
                    state,
                    &lane_id,
                    Utc::now(),
                    ReconcileIds {
                        run_id: Uuid::new_v4(),
                        command_id: Uuid::new_v4(),
                    },
                    coverage,
                )?;
                Ok(())
            })
            .await?;
        }
        Ok(())
    }

    async fn advance_running_phases(&self) -> Result<(), ServiceError> {
        let now = Utc::now();
        let snapshot = self.snapshot().await;
        let runs = snapshot
            .runs
            .values()
            .filter(|run| {
                run.state == RunState::Running && self.executor_kind(&run.lane_id) == "scripted"
            })
            .cloned()
            .collect::<Vec<_>>();
        for run in runs {
            if let Some(next_phase) = self.executor.next_phase(&run, now) {
                self.phase_grant(
                    run.id,
                    run.generation,
                    next_phase,
                    run.executor_ref.clone().unwrap_or_default(),
                )
                .await?;
            } else if run.phase == Phase::Verifying
                && now.signed_duration_since(run.phase_started_at)
                    >= ChronoDuration::milliseconds(
                        i64::try_from(self.config.scripted_phase_ms).unwrap_or(i64::MAX),
                    )
            {
                self.request_verification(run.id, run.generation).await?;
            }
        }
        Ok(())
    }

    async fn request_verification(
        &self,
        run_id: Uuid,
        generation: u64,
    ) -> Result<(), ServiceError> {
        self.transact(None, |engine, state, _| {
            engine.request_verification(state, run_id, generation, Uuid::new_v4(), Utc::now())?;
            Ok(())
        })
        .await
    }

    async fn process_pending_commands(&self) -> Result<(), ServiceError> {
        let _executor = self.executor_gate.lock().await;
        loop {
            let snapshot = self.snapshot().await;
            let may_dispatch = !snapshot.draining && self.ready.load(Ordering::Acquire);
            let Some(command) = snapshot
                .commands
                .values()
                .filter(|command| {
                    command.delivery_state == CommandDeliveryState::Pending
                        && (command.kind != CommandKind::Dispatch || may_dispatch)
                })
                .min_by_key(|command| command.created_at)
                .cloned()
            else {
                break;
            };
            self.process_command(command).await?;
        }
        Ok(())
    }

    async fn process_command(&self, command: Command) -> Result<(), ServiceError> {
        let before = self.snapshot().await;
        if command.kind == CommandKind::Dispatch
            && (before.draining || !self.ready.load(Ordering::Acquire))
        {
            // Leave the durable intent pending for a later explicit resume.
            return Ok(());
        }
        let Some(run) = before.runs.get(&command.run_id).cloned() else {
            return Ok(());
        };
        let configured_kind = self.executor_kind(&run.lane_id);
        let profile_matches = before.lanes.get(&run.lane_id).is_some_and(|lane| {
            lane.profile_digest == run.target.profile_digest
                && lane.repository_id == run.target.repository_id
                && lane.release_line_id == run.target.release_line_id
        });
        if configured_kind == "unconfigured" || !profile_matches {
            let now = Utc::now();
            self.transact(Some(&run.lane_id), |engine, state, _| {
                engine.command_delivery(
                    state,
                    command.id,
                    CommandDelivery::Unknown {
                        reason: "active run does not match its configured executor profile".into(),
                    },
                    now,
                )?;
                Ok(())
            })
            .await?;
            return Ok(());
        }
        let active = before.lanes.get(&run.lane_id).is_some_and(|lane| {
            lane.active_run_id == Some(run.id) && lane.generation == run.generation
        });
        let valid_state = active
            && command.generation == run.generation
            && match &command.kind {
                CommandKind::Dispatch => run.state == RunState::Dispatching,
                CommandKind::Cancel => run.state == RunState::CancelRequested,
                CommandKind::Verify => {
                    run.state == RunState::Running && run.phase == Phase::Verifying
                }
                CommandKind::Observe => false,
            };
        if !valid_state {
            let now = Utc::now();
            self.transact(Some(&run.lane_id), |engine, state, _| {
                engine.command_delivery(
                    state,
                    command.id,
                    CommandDelivery::Unknown {
                        reason: "pending command no longer matches the active run fence".into(),
                    },
                    now,
                )?;
                Ok(())
            })
            .await?;
            return Ok(());
        }
        if self.executor_kind(&run.lane_id) == "hooks" {
            return self.process_hook_command(command, before, run).await;
        }
        let now = Utc::now();
        self.transact(None, |engine, state, _| {
            engine.command_delivery(state, command.id, CommandDelivery::Delivered, now)?;
            Ok(())
        })
        .await?;

        // This executor is synchronous and synthetic. The durable Delivered
        // state precedes it; a crash before the result is committed recovers
        // to Unknown and this command will never be replayed.
        let after_delivery = self.snapshot().await;
        let current_run = after_delivery
            .runs
            .get(&run.id)
            .cloned()
            .unwrap_or(run.clone());
        let result = match &command.kind {
            CommandKind::Dispatch => self.executor.dispatch(&command, &current_run),
            CommandKind::Cancel => self.executor.cancel(&command, &current_run),
            CommandKind::Verify => self.executor.verify(&current_run, Utc::now()),
            CommandKind::Observe => ExecutorResult::Unknown {
                reason: "observe command has no scripted operation".into(),
            },
        };
        let result_time = Utc::now();
        self.transact(Some(&current_run.lane_id), |engine, state, coverage| {
                let stored = state
                    .commands
                    .get(&command.id)
                    .cloned()
                    .ok_or(DomainError::CommandNotFound)?;
                if stored.delivery_state != CommandDeliveryState::Delivered {
                    return Ok(());
                }
                match (&command.kind, result) {
                    (CommandKind::Dispatch, ExecutorResult::Started { executor_ref }) => {
                        engine.command_delivery(
                            state,
                            command.id,
                            CommandDelivery::Acknowledged,
                            result_time,
                        )?;
                        engine.executor_result(
                            state,
                            run.id,
                            command.generation,
                            &executor_ref,
                            ExecutorResult::Started {
                                executor_ref: executor_ref.clone(),
                            },
                            result_time,
                            coverage,
                        )?;
                        engine.record_executor_log(
                            state,
                            run.id,
                            "info",
                            &format!("ScriptedExecutor started generation {}", command.generation),
                            result_time,
                        )?;
                    }
                    (CommandKind::Cancel, ExecutorResult::Stopped { executor_ref }) => {
                        engine.command_delivery(
                            state,
                            command.id,
                            CommandDelivery::Acknowledged,
                            result_time,
                        )?;
                        engine.confirm_stopped(
                            state,
                            run.id,
                            command.generation,
                            &executor_ref,
                            Uuid::new_v4(),
                            result_time,
                            coverage,
                        )?;
                        engine.record_executor_log(
                            state,
                            run.id,
                            "warn",
                            "ScriptedExecutor confirmed a synthetic stop; lane transfer was fenced by generation",
                            result_time,
                        )?;
                    }
                    (CommandKind::Verify, ExecutorResult::VerifiedReceipt(receipt)) => {
                        engine.command_delivery(
                            state,
                            command.id,
                            CommandDelivery::Acknowledged,
                            result_time,
                        )?;
                        let executor_ref = run
                            .executor_ref
                            .clone()
                            .unwrap_or_else(|| "scripted:verification".to_owned());
                        engine.executor_result(
                            state,
                            run.id,
                            command.generation,
                            &executor_ref,
                            ExecutorResult::VerifiedReceipt(receipt),
                            result_time,
                            coverage,
                        )?;
                        engine.record_executor_log(
                            state,
                            run.id,
                            "info",
                            "ScriptedExecutor persisted a synthetic verification receipt; no environment was deployed",
                            result_time,
                        )?;
                    }
                    (kind, ExecutorResult::Failed { message, environment_known_unchanged }) => {
                        engine.command_delivery(
                            state,
                            command.id,
                            CommandDelivery::Acknowledged,
                            result_time,
                        )?;
                        let executor_ref = run.executor_ref.clone().unwrap_or_else(|| {
                            format!("scripted:{}:{}", run.id, command.id)
                        });
                        engine.executor_result(
                            state,
                            run.id,
                            command.generation,
                            &executor_ref,
                            ExecutorResult::Failed {
                                message: bounded_text(&message, 2048),
                                environment_known_unchanged,
                            },
                            result_time,
                            coverage,
                        )?;
                        let _ = kind;
                    }
                    (_, ExecutorResult::Unknown { reason }) => {
                        engine.command_delivery(
                            state,
                            command.id,
                            CommandDelivery::Unknown {
                                reason: bounded_text(&reason, 2048),
                            },
                            result_time,
                        )?;
                    }
                    (_, ExecutorResult::Stopped { .. }) => {
                        engine.command_delivery(
                            state,
                            command.id,
                            CommandDelivery::Unknown {
                                reason: "stop result did not match a cancel command".into(),
                            },
                            result_time,
                        )?;
                    }
                    (_, ExecutorResult::Started { .. }) => {
                        engine.command_delivery(
                            state,
                            command.id,
                            CommandDelivery::Unknown {
                                reason: "start result did not match a dispatch command".into(),
                            },
                            result_time,
                        )?;
                    }
                    (_, ExecutorResult::VerifiedReceipt(_)) => {
                        engine.command_delivery(
                            state,
                            command.id,
                            CommandDelivery::Unknown {
                                reason: "verification receipt did not match a verify command".into(),
                            },
                            result_time,
                        )?;
                    }
                }
                Ok(())
        })
        .await
    }

    async fn process_hook_command(
        &self,
        command: Command,
        mut snapshot: State,
        mut run: crate::domain::DeliveryRun,
    ) -> Result<(), ServiceError> {
        if !self.hook_profile_matches(&snapshot, &run) {
            return self
                .mark_hook_operation_unknown(
                    &run,
                    Some(command.id),
                    "configured hook profile changed while this run was active",
                )
                .await;
        }
        let operation = match &command.kind {
            CommandKind::Dispatch => HookOperation::Dispatch,
            CommandKind::Verify => HookOperation::Verify,
            CommandKind::Cancel | CommandKind::Observe => {
                return self
                    .mark_hook_operation_unknown(
                        &run,
                        Some(command.id),
                        "hook lanes do not support cancel or command-driven observe",
                    )
                    .await;
            }
        };
        if operation == HookOperation::Dispatch {
            self.transact(None, |engine, state, _| {
                engine.begin_external_dispatch(state, run.id, run.generation, Utc::now())?;
                Ok(())
            })
            .await?;
            snapshot = self.snapshot().await;
            run = snapshot
                .runs
                .get(&run.id)
                .cloned()
                .ok_or(DomainError::RunNotFound)?;
        }
        // The durable external-dispatch phase is committed above before the
        // delivery marker and before any configured hook can cause effects.
        let now = Utc::now();
        self.transact(None, |engine, state, _| {
            engine.command_delivery(state, command.id, CommandDelivery::Delivered, now)?;
            Ok(())
        })
        .await?;
        let snapshot = self.snapshot().await;
        let run = snapshot
            .runs
            .get(&run.id)
            .cloned()
            .ok_or(DomainError::RunNotFound)?;
        let lane = snapshot
            .lanes
            .get(&run.lane_id)
            .cloned()
            .ok_or_else(|| DomainError::LaneNotFound(run.lane_id.clone()))?;
        let config = self
            .hook_config(&run.lane_id, operation)
            .ok_or_else(|| anyhow::anyhow!("hook operation is not configured"))?;
        let envelope = HookEnvelope {
            version: 1,
            operation,
            command: Some(&command),
            run: &run,
            lane: &lane,
        };
        match HookExecutor::call(&config, &envelope).await {
            Ok(HookCall::Accepted) => Ok(()),
            Ok(HookCall::Reply(reply)) => {
                self.apply_hook_reply(operation, Some(&command), &run, *reply)
                    .await
            }
            Err(error) => {
                self.mark_hook_operation_unknown(
                    &run,
                    Some(command.id),
                    &format!("configured hook result is unproven: {error}"),
                )
                .await
            }
        }
    }

    async fn poll_hook_runs(&self) -> Result<(), ServiceError> {
        let _executor = self.executor_gate.lock().await;
        let snapshot = self.snapshot().await;
        let active = snapshot
            .runs
            .values()
            .filter(|run| {
                run.state.owns_execution_slot() && self.executor_kind(&run.lane_id) == "hooks"
            })
            .cloned()
            .collect::<Vec<_>>();
        for run in active {
            let state = self.snapshot().await;
            let Some(current) = state.runs.get(&run.id).cloned() else {
                continue;
            };
            let Some(lane) = state.lanes.get(&current.lane_id).cloned() else {
                continue;
            };
            if current.state == RunState::Dispatching
                && state.commands.values().any(|command| {
                    command.run_id == current.id
                        && command.generation == current.generation
                        && command.kind == CommandKind::Dispatch
                        && command.delivery_state == CommandDeliveryState::Pending
                })
            {
                continue;
            }
            if !self.hook_profile_matches(&state, &current) {
                self.mark_hook_operation_unknown(
                    &current,
                    None,
                    "configured hook profile changed while this run was active",
                )
                .await?;
                continue;
            }
            let Some(config) = self.hook_config(&current.lane_id, HookOperation::Observe) else {
                self.mark_hook_operation_unknown(
                    &current,
                    None,
                    "observe hook is not configured for the active run",
                )
                .await?;
                continue;
            };
            let envelope = HookEnvelope {
                version: 1,
                operation: HookOperation::Observe,
                command: None,
                run: &current,
                lane: &lane,
            };
            match HookExecutor::call(&config, &envelope).await {
                Ok(HookCall::Accepted) => {}
                Ok(HookCall::Reply(reply)) => {
                    let completed = reply.status == HookStatus::Completed;
                    self.apply_hook_reply(HookOperation::Observe, None, &current, *reply)
                        .await?;
                    if completed {
                        self.retry_delivered_verification(current.id).await?;
                    }
                }
                Err(error) => {
                    self.mark_hook_operation_unknown(
                        &current,
                        None,
                        &format!("hook observation is unproven: {error}"),
                    )
                    .await?;
                }
            }
            self.observe_failed_stop(&current).await?;
        }
        Ok(())
    }

    async fn retry_delivered_verification(&self, run_id: Uuid) -> Result<(), ServiceError> {
        let snapshot = self.snapshot().await;
        let Some(run) = snapshot.runs.get(&run_id).cloned() else {
            return Ok(());
        };
        if run.state != RunState::Running || run.phase != Phase::Verifying {
            return Ok(());
        }
        let command = snapshot
            .commands
            .values()
            .filter(|command| {
                command.run_id == run.id
                    && command.generation == run.generation
                    && command.kind == CommandKind::Verify
            })
            .max_by_key(|command| command.created_at)
            .cloned();
        let Some(command) = command else {
            return Ok(());
        };
        if matches!(
            command.delivery_state,
            CommandDeliveryState::Pending | CommandDeliveryState::Acknowledged
        ) {
            return Ok(());
        }
        // Verify is an independent read-only evidence check. A delivered or
        // unknown verification may be repeated after a fresh completed
        // observation; dispatch is never replayed.
        let lane = snapshot
            .lanes
            .get(&run.lane_id)
            .cloned()
            .ok_or_else(|| DomainError::LaneNotFound(run.lane_id.clone()))?;
        let config = self
            .hook_config(&run.lane_id, HookOperation::Verify)
            .ok_or_else(|| anyhow::anyhow!("verify hook is not configured"))?;
        let envelope = HookEnvelope {
            version: 1,
            operation: HookOperation::Verify,
            command: Some(&command),
            run: &run,
            lane: &lane,
        };
        match HookExecutor::call(&config, &envelope).await {
            Ok(HookCall::Accepted) => Ok(()),
            Ok(HookCall::Reply(reply)) => {
                self.apply_hook_reply(HookOperation::Verify, Some(&command), &run, *reply)
                    .await
            }
            Err(error) => {
                self.mark_hook_operation_unknown(
                    &run,
                    Some(command.id),
                    &format!("fresh verification is unproven: {error}"),
                )
                .await
            }
        }
    }

    async fn apply_hook_reply(
        &self,
        operation: HookOperation,
        command: Option<&Command>,
        observed_run: &crate::domain::DeliveryRun,
        reply: HookReply,
    ) -> Result<(), ServiceError> {
        let run_id = observed_run.id;
        let now = Utc::now();
        let reply_digest = hex::encode(Sha256::digest(
            serde_json::to_vec(&reply).unwrap_or_else(|_| b"invalid-hook-reply".to_vec()),
        ));
        self.transact(Some(&observed_run.lane_id), |engine, state, coverage| {
            let run = state
                .runs
                .get(&run_id)
                .cloned()
                .ok_or(DomainError::RunNotFound)?;
            if run.generation != reply.generation
                || run.id != reply.run_id
                || state.lanes.get(&run.lane_id).is_none_or(|lane| {
                    run.target.profile_digest != lane.profile_digest
                        || run.target.repository_id != lane.repository_id
                        || run.target.release_line_id != lane.release_line_id
                })
            {
                if run.state != RunState::Unknown {
                    engine.mark_unknown(
                        state,
                        run.id,
                        run.generation,
                        "hook reply profile or generation no longer matches the active lane",
                        now,
                    )?;
                }
                return Ok(());
            }
            let supplied_ref = reply.executor_ref.as_deref();
            if let Some(expected) = run.executor_ref.as_deref()
                && supplied_ref.is_some_and(|value| value != expected)
            {
                engine.mark_unknown(
                    state,
                    run.id,
                    run.generation,
                    "hook reply changed the stable executor reference",
                    now,
                )?;
                if let Some(command) = command {
                    engine.command_delivery(
                        state,
                        command.id,
                        CommandDelivery::Unknown {
                            reason: "hook executor reference mismatch".into(),
                        },
                        now,
                    )?;
                }
                return Ok(());
            }
            store_hook_logs(
                engine,
                state,
                run.id,
                supplied_ref.unwrap_or("unassigned"),
                &reply.logs,
                now,
            )?;

            let executor_ref = supplied_ref
                .map(str::to_owned)
                .or_else(|| run.executor_ref.clone())
                .unwrap_or_else(|| format!("hook:{}:{}", run.id, run.generation));
            if matches!(operation, HookOperation::Dispatch | HookOperation::Observe)
                && let Some(dispatch) =
                    command.filter(|command| command.kind == CommandKind::Dispatch)
            {
                engine.command_delivery(state, dispatch.id, CommandDelivery::Acknowledged, now)?;
            } else if operation == HookOperation::Observe
                && let Some(dispatch) = state
                    .commands
                    .values()
                    .filter(|command| {
                        command.run_id == run.id
                            && command.generation == run.generation
                            && command.kind == CommandKind::Dispatch
                    })
                    .max_by_key(|command| command.created_at)
                    .cloned()
                && dispatch.delivery_state != CommandDeliveryState::Acknowledged
            {
                engine.command_delivery(state, dispatch.id, CommandDelivery::Acknowledged, now)?;
            }

            match reply.status {
                HookStatus::Started | HookStatus::Running => {
                    if operation == HookOperation::Verify || supplied_ref.is_none() {
                        engine.mark_unknown(
                            state,
                            run.id,
                            run.generation,
                            "hook active-state reply omitted its stable executor reference",
                            now,
                        )?;
                    } else if run.state == RunState::Unknown {
                        engine.resolve_unknown(
                            state,
                            run.id,
                            run.generation,
                            UnknownResolution::KnownRunning {
                                executor_ref: executor_ref.clone(),
                                evidence_digest: reply_digest.clone(),
                            },
                            Uuid::new_v4(),
                            now,
                            coverage,
                        )?;
                    } else if run.state == RunState::Dispatching {
                        engine.executor_result(
                            state,
                            run.id,
                            run.generation,
                            &executor_ref,
                            ExecutorResult::Started {
                                executor_ref: executor_ref.clone(),
                            },
                            now,
                            coverage,
                        )?;
                    }
                }
                HookStatus::Completed => {
                    if operation == HookOperation::Verify || supplied_ref.is_none() {
                        engine.mark_unknown(
                            state,
                            run.id,
                            run.generation,
                            "hook completion reply omitted its stable executor reference",
                            now,
                        )?;
                    } else {
                        if run.state == RunState::Unknown {
                            engine.resolve_unknown(
                                state,
                                run.id,
                                run.generation,
                                UnknownResolution::KnownRunning {
                                    executor_ref: executor_ref.clone(),
                                    evidence_digest: reply_digest.clone(),
                                },
                                Uuid::new_v4(),
                                now,
                                coverage,
                            )?;
                        } else if run.state == RunState::Dispatching {
                            engine.executor_result(
                                state,
                                run.id,
                                run.generation,
                                &executor_ref,
                                ExecutorResult::Started {
                                    executor_ref: executor_ref.clone(),
                                },
                                now,
                                coverage,
                            )?;
                        }
                        advance_hook_to_verifying(
                            engine,
                            state,
                            run.id,
                            run.generation,
                            &executor_ref,
                            now,
                        )?;
                        engine.request_verification(
                            state,
                            run.id,
                            run.generation,
                            Uuid::new_v4(),
                            now,
                        )?;
                    }
                }
                HookStatus::Failed => {
                    if reply.failure_proof.is_some() {
                        let evidence = serde_json::to_string(&serde_json::json!({
                            "run_id": reply.run_id, "generation": reply.generation,
                            "executor_ref": reply.executor_ref, "proof": reply.failure_proof,
                        }))
                        .map_err(|_| ServiceError::UnprovenExecutorState)?;
                        engine.confirm_failed_proven(
                            state,
                            run.id,
                            run.generation,
                            &executor_ref,
                            &evidence,
                            &bounded_text(
                                reply
                                    .message
                                    .as_deref()
                                    .unwrap_or("executor stopped after failure"),
                                1024,
                            ),
                            now,
                        )?;
                    } else if operation == HookOperation::Verify {
                        if let Some(command) = command {
                            engine.command_delivery(
                                state,
                                command.id,
                                CommandDelivery::Unknown {
                                    reason:
                                        "verification hook failed to produce independent evidence"
                                            .into(),
                                },
                                now,
                            )?;
                        } else if run.state != RunState::Unknown {
                            engine.mark_unknown(
                                state,
                                run.id,
                                run.generation,
                                "verification hook failed to produce independent evidence",
                                now,
                            )?;
                        }
                    } else {
                        engine.executor_result(
                            state,
                            run.id,
                            run.generation,
                            &executor_ref,
                            ExecutorResult::Failed {
                                message: bounded_text(
                                    reply.message.as_deref().unwrap_or("hook failed"),
                                    2048,
                                ),
                                environment_known_unchanged: reply.environment_known_unchanged
                                    == Some(true),
                            },
                            now,
                            coverage,
                        )?;
                    }
                }
                HookStatus::Unknown => {
                    if operation == HookOperation::Verify
                        && let Some(command) = command
                    {
                        engine.command_delivery(
                            state,
                            command.id,
                            CommandDelivery::Unknown {
                                reason: "verification hook did not prove a receipt".into(),
                            },
                            now,
                        )?;
                    } else if run.state != RunState::Unknown {
                        engine.mark_unknown(
                            state,
                            run.id,
                            run.generation,
                            &bounded_text(
                                reply
                                    .message
                                    .as_deref()
                                    .unwrap_or("hook observation is unknown"),
                                1024,
                            ),
                            now,
                        )?;
                    }
                }
                HookStatus::Verified => {
                    let Some(receipt) = reply.receipt.clone() else {
                        engine.mark_unknown(
                            state,
                            run.id,
                            run.generation,
                            "verify hook omitted receipt",
                            now,
                        )?;
                        return Ok(());
                    };
                    let Some(command) =
                        command.filter(|command| command.kind == CommandKind::Verify)
                    else {
                        engine.mark_unknown(
                            state,
                            run.id,
                            run.generation,
                            "verified reply had no durable verify command",
                            now,
                        )?;
                        return Ok(());
                    };
                    engine.command_delivery(
                        state,
                        command.id,
                        CommandDelivery::Acknowledged,
                        now,
                    )?;
                    if run.state == RunState::Unknown {
                        engine.resolve_unknown(
                            state,
                            run.id,
                            run.generation,
                            UnknownResolution::VerifiedReceipt {
                                receipt,
                                evidence_digest: reply_digest,
                            },
                            Uuid::new_v4(),
                            now,
                            coverage,
                        )?;
                    } else {
                        engine.executor_result(
                            state,
                            run.id,
                            run.generation,
                            &executor_ref,
                            ExecutorResult::VerifiedReceipt(receipt),
                            now,
                            coverage,
                        )?;
                    }
                }
            }
            let message = bounded_text(reply.message.as_deref().unwrap_or(""), 1024);
            let signature = hex::encode(Sha256::digest(
                serde_json::to_vec(&serde_json::json!({
                    "status": reply.status, "executor_ref": reply.executor_ref, "message": message,
                }))
                .expect("observation summary serializes"),
            ));
            engine.record_hook_observation(
                state,
                run.id,
                &format!("{operation:?}"),
                &signature,
                if reply.status == HookStatus::Failed {
                    "error"
                } else {
                    "info"
                },
                &message,
                now,
            )?;
            Ok(())
        })
        .await
    }

    async fn mark_hook_operation_unknown(
        &self,
        run: &crate::domain::DeliveryRun,
        command_id: Option<Uuid>,
        reason: &str,
    ) -> Result<(), ServiceError> {
        let reason = bounded_text(reason, 1024);
        let now = Utc::now();
        self.transact(Some(&run.lane_id), |engine, state, _| {
            if let Some(command_id) = command_id
                && state.commands.get(&command_id).is_some_and(|command| {
                    command.delivery_state != CommandDeliveryState::Acknowledged
                })
            {
                engine.command_delivery(
                    state,
                    command_id,
                    CommandDelivery::Unknown {
                        reason: reason.clone(),
                    },
                    now,
                )?;
            } else if state
                .runs
                .get(&run.id)
                .is_some_and(|current| current.state != RunState::Unknown)
            {
                engine.mark_unknown(state, run.id, run.generation, &reason, now)?;
            }
            engine.record_hook_observation(
                state,
                run.id,
                "transport",
                &reason,
                "warn",
                &reason,
                now,
            )?;
            Ok(())
        })
        .await
    }

    fn hook_config(&self, lane_id: &str, operation: HookOperation) -> Option<HookConfig> {
        let (dispatch, observe, verify) = self.lane_executor(lane_id)?.hooks()?;
        Some(match operation {
            HookOperation::Dispatch => dispatch.clone(),
            HookOperation::Observe => observe.clone(),
            HookOperation::Verify => verify.clone(),
        })
    }

    fn hook_profile_matches(&self, state: &State, run: &crate::domain::DeliveryRun) -> bool {
        self.executor_kind(&run.lane_id) == "hooks"
            && state.lanes.get(&run.lane_id).is_some_and(|lane| {
                lane.profile_digest == run.target.profile_digest
                    && lane.repository_id == run.target.repository_id
                    && lane.release_line_id == run.target.release_line_id
            })
    }

    async fn executor_kind_for_run(&self, run_id: Uuid) -> Option<&'static str> {
        let snapshot = self.snapshot().await;
        let run = snapshot.runs.get(&run_id)?;
        Some(self.executor_kind(&run.lane_id))
    }

    async fn observe_failed_stop(
        &self,
        run: &crate::domain::DeliveryRun,
    ) -> Result<bool, ServiceError> {
        let state = self.snapshot().await;
        let Some(current) = state.runs.get(&run.id) else {
            return Ok(false);
        };
        if current.state != RunState::Unknown || !self.hook_profile_matches(&state, current) {
            return Ok(false);
        }
        let Some(config) = self
            .config
            .lanes
            .iter()
            .find(|lane| lane.id == current.lane_id)
            .and_then(|lane| lane.failure_observe.clone())
        else {
            return Ok(false);
        };
        let Some(lane) = state.lanes.get(&current.lane_id) else {
            return Ok(false);
        };
        let envelope = HookEnvelope {
            version: 1,
            operation: HookOperation::Observe,
            command: None,
            run: current,
            lane,
        };
        let Ok(HookCall::Reply(mut reply)) = HookExecutor::call(&config, &envelope).await else {
            return Ok(false);
        };
        if reply.status != HookStatus::Failed || reply.failure_proof.is_none() {
            return Ok(false);
        }
        // Preserve which trusted failure-only observer produced the proof.
        if let Some(evidence) = reply
            .failure_proof
            .as_mut()
            .and_then(|proof| proof.evidence.as_object_mut())
        {
            evidence.insert(
                "observer_config_sha256".into(),
                serde_json::json!(hex::encode(Sha256::digest(
                    serde_json::to_vec(&config).expect("hook config serializes")
                ))),
            );
        }
        self.apply_hook_reply(HookOperation::Observe, None, current, *reply)
            .await?;
        Ok(true)
    }

    async fn resolve_unknown_hook(
        &self,
        snapshot: &State,
        lane: &Lane,
        run: &crate::domain::DeliveryRun,
        action: ResolveAction,
        evidence_note: String,
        reason: String,
    ) -> Result<(), ServiceError> {
        if !self.hook_profile_matches(snapshot, run) {
            return Err(ServiceError::UnprovenExecutorState);
        }
        if matches!(action, ResolveAction::ConfirmStopped) && self.observe_failed_stop(run).await? {
            return Ok(());
        }
        let operation = if matches!(action, ResolveAction::RecordVerified) {
            HookOperation::Verify
        } else {
            HookOperation::Observe
        };
        let config = self
            .hook_config(&lane.id, operation)
            .ok_or(ServiceError::UnprovenExecutorState)?;
        let envelope = HookEnvelope {
            version: 1,
            operation,
            command: None,
            run,
            lane,
        };
        let HookCall::Reply(reply) = HookExecutor::call(&config, &envelope)
            .await
            .map_err(|_| ServiceError::UnprovenExecutorState)?
        else {
            return Err(ServiceError::UnprovenExecutorState);
        };
        if matches!(action, ResolveAction::ConfirmStopped) {
            if reply.status != HookStatus::Failed || reply.failure_proof.is_none() {
                return Err(ServiceError::UnprovenExecutorState);
            }
            return self
                .apply_hook_reply(HookOperation::Observe, None, run, *reply)
                .await;
        }
        let proof = match (
            &action,
            reply.status,
            reply.executor_ref.clone(),
            reply.receipt.clone(),
        ) {
            (
                ResolveAction::AdoptRunning,
                HookStatus::Started | HookStatus::Running | HookStatus::Completed,
                Some(executor_ref),
                _,
            ) => UnknownResolution::KnownRunning {
                executor_ref,
                evidence_digest: hook_evidence_digest(&reply),
            },
            (ResolveAction::RecordVerified, HookStatus::Verified, _, Some(receipt)) => {
                UnknownResolution::VerifiedReceipt {
                    receipt,
                    evidence_digest: hook_evidence_digest(&reply),
                }
            }
            _ => return Err(ServiceError::UnprovenExecutorState),
        };
        let command_id = Uuid::new_v4();
        let now = Utc::now();
        self.transact(Some(&lane.id), |engine, state, coverage| {
            engine.resolve_unknown(
                state,
                run.id,
                run.generation,
                proof,
                command_id,
                now,
                coverage,
            )?;
            let note = bounded_text(&evidence_note, 1024);
            let reason = bounded_text(&reason, 512);
            let _ = engine.record_executor_log(
                state,
                run.id,
                "warn",
                &format!("admin hook observation; reason={reason}; note={note}"),
                now,
            );
            Ok(())
        })
        .await
    }

    async fn check_cancel_timeouts(&self) -> Result<(), ServiceError> {
        let now = Utc::now();
        let cutoff = now
            - ChronoDuration::seconds(
                i64::try_from(self.config.cancel_timeout_seconds).unwrap_or(i64::MAX),
            );
        let snapshot = self.snapshot().await;
        let expired = snapshot
            .runs
            .values()
            .filter(|run| run.state == RunState::CancelRequested && run.updated_at <= cutoff)
            .cloned()
            .collect::<Vec<_>>();
        for run in expired {
            self.transact(None, |engine, state, _| {
                let current = state.runs.get(&run.id).ok_or(DomainError::RunNotFound)?;
                if current.state == RunState::CancelRequested && current.updated_at <= cutoff {
                    engine.mark_unknown(
                        state,
                        run.id,
                        run.generation,
                        "scripted stop confirmation timed out; lane remains fenced",
                        now,
                    )?;
                }
                Ok(())
            })
            .await?;
        }
        Ok(())
    }
}

pub async fn serve(config: Config) -> anyhow::Result<()> {
    let service = Service::open(config.clone()).await?;
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("bind HTTP listener at {}", config.listen))?;
    let scheduler_service = service.clone();
    let scheduler = tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_millis(scheduler_service.config.tick_ms));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if let Err(error) = scheduler_service.scheduler_tick().await {
                tracing::error!(error = %error, "scheduler tick failed");
            }
        }
    });
    let shutdown_service = service.clone();
    let shutdown_gate = service.clone();
    let scheduler_abort = scheduler.abort_handle();
    let serve_result = axum::serve(listener, api::router(service))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            // Stop new ticks before waiting for in-flight HTTP requests. The
            // operator's durable drain preference is not changed by SIGTERM.
            shutdown_gate.ready.store(false, Ordering::Release);
            scheduler_abort.abort();
        })
        .await;
    shutdown_service.ready.store(false, Ordering::Release);
    scheduler.abort();
    let _ = scheduler.await;
    serve_result.context("HTTP server stopped")?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    tracing::error!(error = %error, "failed to listen for Ctrl-C");
                }
            }
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(error = %error, "failed to listen for Ctrl-C");
    }
}

fn canonical_database_path(path: &Path) -> anyhow::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("database path must not be empty");
    }
    if path.exists() {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("resolve database file {}", path.display()))?;
        if !canonical.is_file() {
            bail!("database path is not a file: {}", canonical.display());
        }
        return Ok(canonical);
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create database directory {}", parent.display()))?;
    let parent = parent
        .canonicalize()
        .with_context(|| format!("resolve database directory {}", parent.display()))?;
    let filename = path.file_name().context("database path must name a file")?;
    Ok(parent.join(filename))
}

struct DatabaseLock(File);

impl Drop for DatabaseLock {
    fn drop(&mut self) {
        // Explicit unlock also releases the open-file-description lock if a
        // concurrent fork briefly inherited this FD before its CLOEXEC close.
        let _ = FileExt::unlock(&self.0);
    }
}

fn acquire_database_lock(database_path: &Path) -> anyhow::Result<DatabaseLock> {
    let mut lock_name: OsString = database_path.as_os_str().to_owned();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open database lock {}", lock_path.display()))?;
    lock_file.try_lock_exclusive().with_context(|| {
        format!(
            "another shipbraid process already owns database {}",
            database_path.display()
        )
    })?;
    Ok(DatabaseLock(lock_file))
}

fn release_line_id(lane: &crate::config::LaneConfig) -> String {
    let path = lane
        .repository_path
        .canonicalize()
        .unwrap_or_else(|_| lane.repository_path.clone());
    let material = format!(
        "{}\0{}\0{}",
        lane.repository_id,
        path.display(),
        lane.release_ref
    );
    format!(
        "{}:{}",
        lane.repository_id,
        hex::encode(Sha256::digest(material.as_bytes()))[..16].to_owned()
    )
}

fn lane_config_matches(existing: &Lane, desired: &Lane) -> bool {
    existing.project == desired.project
        && existing.environment == desired.environment
        && existing.enabled == desired.enabled
        && existing.repository_id == desired.repository_id
        && existing.release_ref == desired.release_ref
        && existing.release_line_id == desired.release_line_id
        && existing.profile_digest == desired.profile_digest
        && existing.descendant_coverage_enabled == desired.descendant_coverage_enabled
        && existing.preemption_enabled == desired.preemption_enabled
        && existing.allowed_revisions == desired.allowed_revisions
        && existing.allowed_components == desired.allowed_components
}

fn bounded_text(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || *character == '\t')
        .take(limit)
        .collect()
}

fn store_hook_logs(
    engine: &Engine,
    state: &mut State,
    run_id: Uuid,
    _executor_ref: &str,
    logs: &[crate::hooks::HookLog],
    now: chrono::DateTime<Utc>,
) -> Result<(), DomainError> {
    for log in logs {
        let marker = format!("[hook_seq:{}]", log.sequence);
        if state.events.iter().any(|event| {
            event.run_id == Some(run_id)
                && event.kind == "executor_log"
                && event.message.contains(&marker)
        }) {
            continue;
        }
        let message = bounded_text(&log.message, 1800);
        if message.is_empty() {
            continue;
        }
        engine.record_executor_log(
            state,
            run_id,
            &log.level,
            &format!("{marker} {message}"),
            now,
        )?;
    }
    Ok(())
}

fn advance_hook_to_verifying(
    engine: &Engine,
    state: &mut State,
    run_id: Uuid,
    generation: u64,
    executor_ref: &str,
    now: chrono::DateTime<Utc>,
) -> Result<(), DomainError> {
    loop {
        let phase = state
            .runs
            .get(&run_id)
            .map(|run| run.phase.clone())
            .ok_or(DomainError::RunNotFound)?;
        let next = match phase {
            Phase::Preparing => Phase::Building,
            Phase::Building => Phase::Deploying,
            Phase::Deploying => Phase::Verifying,
            Phase::Verifying => return Ok(()),
        };
        engine.phase_grant(state, run_id, generation, executor_ref, next, now)?;
    }
}

fn hook_evidence_digest(reply: &HookReply) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(reply).unwrap_or_else(|_| b"invalid-hook-reply".to_vec()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{LaneConfig, TokenConfig},
        domain::{LineageRelation, ReconcileOutcome, SubmitInput, TargetAuthorization},
    };

    async fn test_service() -> (tempfile::TempDir, Arc<Service>) {
        let temp = tempfile::tempdir().unwrap();
        let mirror = temp.path().join("source.git");
        let output = std::process::Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&mirror)
            .output()
            .unwrap();
        assert!(output.status.success(), "git init failed: {:?}", output);
        let config = Config {
            database_path: temp.path().join("ledger.sqlite3"),
            listen: "127.0.0.1:0".parse().unwrap(),
            tokens: vec![TokenConfig {
                id: "test-admin".into(),
                token_env: "PATH".into(),
                lanes: vec!["test-lane".into()],
                projects: vec![],
                urgent: true,
                admin: true,
                executor: false,
            }],
            lanes: vec![LaneConfig {
                id: "test-lane".into(),
                project: "test-project".into(),
                environment: "test".into(),
                repository_id: "test-repository".into(),
                repository_path: mirror,
                release_ref: "refs/heads/main".into(),
                profile_digest: "profile-v1".into(),
                components: vec!["app".into()],
                allow_descendant: false,
                allow_preemption: false,
                failure_observe: None,
                enabled: true,
                executor: LaneExecutorConfig::Scripted,
            }],
            max_active_runs: 1,
            tick_ms: 1_000,
            scripted_phase_ms: 60_000,
            cancel_timeout_seconds: 30,
            coalesce_ms: 0,
            dashboard_dir: None,
        };
        let service = Service::open(config).await.unwrap();
        (temp, service)
    }

    async fn hook_test_service() -> (tempfile::TempDir, Config, Arc<Service>, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let mirror = temp.path().join("source.git");
        let output = std::process::Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&mirror)
            .output()
            .unwrap();
        assert!(output.status.success(), "git init failed: {:?}", output);
        let marker = temp.path().join("dispatch-count.txt");
        let script = temp.path().join("hook.py");
        std::fs::write(
            &script,
            r#"import datetime, json, sys, uuid
request = json.load(sys.stdin)
run = request['run']
operation = request['operation']
executor_ref = 'fixture:' + run['id']
reply = {'version': 1, 'run_id': run['id'], 'generation': run['generation'], 'executor_ref': executor_ref, 'status': 'started', 'logs': []}
if operation == 'dispatch':
    with open(sys.argv[1], 'a', encoding='utf-8') as output:
        output.write('dispatch\n')
    reply['status'] = 'started'
elif operation == 'observe':
    reply['status'] = 'completed'
elif operation == 'verify':
    reply['status'] = 'verified'
    reply['receipt'] = {
        'id': str(uuid.uuid4()),
        'run_id': run['id'],
        'generation': run['generation'],
        'observed_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
        'source_revision': run['target']['revision'],
        'component_digests': {component: 'fixture-digest' for component in run['target']['components']},
        'profile_digest': run['target']['profile_digest'],
        'verification_report': 'independent fixture verification',
        'result': 'success'
    }
print(json.dumps(reply))
"#,
        )
        .unwrap();
        let hook = HookConfig::Script {
            path: script,
            interpreter: "python3".into(),
            args: vec![marker.display().to_string()],
            cwd: None,
            timeout_ms: 5_000,
        };
        let config = Config {
            database_path: temp.path().join("ledger.sqlite3"),
            listen: "127.0.0.1:0".parse().unwrap(),
            tokens: vec![TokenConfig {
                id: "test-admin".into(),
                token_env: "PATH".into(),
                lanes: vec!["test-lane".into()],
                projects: vec![],
                urgent: true,
                admin: true,
                executor: false,
            }],
            lanes: vec![LaneConfig {
                id: "test-lane".into(),
                project: "test-project".into(),
                environment: "dev".into(),
                repository_id: "test-repository".into(),
                repository_path: mirror,
                release_ref: "refs/heads/main".into(),
                profile_digest: "profile-v1".into(),
                components: vec!["app".into()],
                allow_descendant: false,
                allow_preemption: false,
                failure_observe: None,
                enabled: true,
                executor: LaneExecutorConfig::Hooks {
                    dispatch: hook.clone(),
                    observe: hook.clone(),
                    verify: hook,
                },
            }],
            max_active_runs: 1,
            tick_ms: 1_000,
            scripted_phase_ms: 60_000,
            cancel_timeout_seconds: 30,
            coalesce_ms: 0,
            dashboard_dir: None,
        };
        let service = Service::open(config.clone()).await.unwrap();
        (temp, config, service, marker)
    }

    async fn create_pending_dispatch(service: &Service) -> (Uuid, Uuid) {
        service
            .transact(Some("test-lane"), |engine, state, coverage| {
                let lane = state.lanes.get("test-lane").unwrap().clone();
                let revision = "a".repeat(40);
                let now = Utc::now();
                let target = Target {
                    repository_id: lane.repository_id.clone(),
                    revision: revision.clone(),
                    components: ["app".to_owned()].into_iter().collect(),
                    profile_digest: lane.profile_digest.clone(),
                    release_line_id: lane.release_line_id.clone(),
                    config_version: lane.config_version,
                    coverage_mode: CoverageMode::Exact,
                };
                engine.submit(
                    state,
                    SubmitInput {
                        id: Uuid::new_v4(),
                        principal: "test-admin".into(),
                        context: RequestContext::default(),
                        idempotency_key: "test-submit".into(),
                        request_hash: "test-request-hash".into(),
                        lane_id: lane.id.clone(),
                        target: target.clone(),
                        authorization: TargetAuthorization {
                            repository_id: lane.repository_id,
                            release_ref: lane.release_ref,
                            release_tip: revision.clone(),
                            revision,
                            release_line_id: lane.release_line_id,
                            config_version: lane.config_version,
                            relation: LineageRelation::Ancestor,
                            proof_digest: "fixture-lineage-proof".into(),
                        },
                        priority: Priority::Normal,
                        deadline_at: None,
                        reason: "service test".into(),
                    },
                    now,
                    coverage,
                )?;
                let run_id = Uuid::new_v4();
                let command_id = Uuid::new_v4();
                let outcome = engine.reconcile(
                    state,
                    &lane.id,
                    now,
                    ReconcileIds { run_id, command_id },
                    coverage,
                )?;
                assert!(matches!(outcome, ReconcileOutcome::Dispatched { .. }));
                Ok((run_id, command_id))
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn retry_and_action_hints_enforce_urgent_lane_deadline_and_limit() {
        let (_temp, service) = test_service().await;
        create_pending_dispatch(&service).await;
        let request_id = *service.snapshot().await.requests.keys().next().unwrap();
        service
            .transact(None, |_, state, _| {
                let request = state.requests.get_mut(&request_id).unwrap();
                request.status = RequestStatus::NeedsAttention;
                request.priority = Priority::Urgent;
                request.context.collaborators.insert("collaborator".into());
                Ok(())
            })
            .await
            .unwrap();
        let mut actor = RequestActor {
            id: "collaborator".into(),
            lanes: vec!["test-lane".into()],
            projects: vec![],
            urgent: false,
            admin: false,
            executor: false,
        };
        let snapshot = service.snapshot().await;
        let request = &snapshot.requests[&request_id];
        assert_eq!(
            request_allowed_actions(&actor, request, &snapshot, Utc::now()),
            vec!["withdraw"]
        );
        assert!(matches!(
            service
                .mutate_request(request_id, &actor, RequestMutation::Retry)
                .await,
            Err(ServiceError::Forbidden)
        ));
        actor.urgent = true;
        actor.lanes.clear();
        assert!(matches!(
            service
                .mutate_request(request_id, &actor, RequestMutation::Retry)
                .await,
            Err(ServiceError::Forbidden)
        ));
        actor.lanes.push("test-lane".into());
        assert!(request_allowed_actions(&actor, request, &snapshot, Utc::now()).contains(&"retry"));
        let mut limited = request.clone();
        limited.manual_retries_used = MAX_MANUAL_RETRIES;
        assert!(
            !request_allowed_actions(&actor, &limited, &snapshot, Utc::now()).contains(&"retry")
        );
        let mut expired = request.clone();
        expired.deadline_at = Some(Utc::now() - ChronoDuration::seconds(1));
        assert!(
            !request_allowed_actions(&actor, &expired, &snapshot, Utc::now()).contains(&"retry")
        );
        actor.executor = true;
        assert!(request_allowed_actions(&actor, request, &snapshot, Utc::now()).is_empty());
        actor.executor = false;
        service
            .mutate_request(request_id, &actor, RequestMutation::Retry)
            .await
            .unwrap();
        let snapshot = service.snapshot().await;
        assert_eq!(
            snapshot.requests[&request_id].status,
            RequestStatus::Waiting
        );
        assert_eq!(
            snapshot.events.last().unwrap().actor.as_deref(),
            Some("collaborator")
        );
        assert!(matches!(
            service
                .mutate_request(request_id, &actor, RequestMutation::Retry)
                .await,
            Err(ServiceError::Domain(DomainError::NotRetryable))
        ));
    }

    #[test]
    fn database_lock_paths_are_canonical_and_stable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("data").join("delivery.db");
        let canonical = canonical_database_path(&path).unwrap();
        let first = acquire_database_lock(&canonical).unwrap();
        assert!(acquire_database_lock(&canonical).is_err());
        drop(first);
        assert!(acquire_database_lock(&canonical).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_database_paths_share_the_canonical_lock() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real.db");
        std::fs::write(&real, b"test").unwrap();
        let alias = temp.path().join("alias.db");
        symlink(&real, &alias).unwrap();

        let canonical_real = canonical_database_path(&real).unwrap();
        let canonical_alias = canonical_database_path(&alias).unwrap();
        assert_eq!(canonical_real, canonical_alias);
        let lock = acquire_database_lock(&canonical_real).unwrap();
        assert!(acquire_database_lock(&canonical_alias).is_err());
        drop(lock);
    }

    #[tokio::test]
    async fn drain_defers_pending_dispatch_until_explicit_resume() {
        let (_temp, service) = test_service().await;
        let (run_id, command_id) = create_pending_dispatch(&service).await;

        service.set_draining(true).await.unwrap();
        service.process_pending_commands().await.unwrap();

        let drained = service.snapshot().await;
        assert!(drained.draining);
        assert_eq!(
            drained.commands[&command_id].delivery_state,
            CommandDeliveryState::Pending
        );
        assert_eq!(drained.runs[&run_id].state, RunState::Dispatching);
        assert_eq!(
            ScriptedExecutor::synthetic_physical_start_count(&drained),
            0
        );

        service.set_draining(false).await.unwrap();
        service.process_pending_commands().await.unwrap();

        let resumed = service.snapshot().await;
        assert!(!resumed.draining);
        assert_eq!(
            resumed.commands[&command_id].delivery_state,
            CommandDeliveryState::Acknowledged
        );
        assert_eq!(resumed.runs[&run_id].state, RunState::Running);
        assert_eq!(
            ScriptedExecutor::synthetic_physical_start_count(&resumed),
            1
        );
    }

    #[tokio::test]
    async fn started_replay_finds_dispatch_after_verify_command_exists() {
        let (_temp, service) = test_service().await;
        let (run_id, _) = create_pending_dispatch(&service).await;
        service.process_pending_commands().await.unwrap();
        let mut run = service.snapshot().await.runs[&run_id].clone();
        let executor_ref = run.executor_ref.clone().unwrap();
        for phase in [Phase::Building, Phase::Deploying, Phase::Verifying] {
            service
                .phase_grant(run_id, run.generation, phase.clone(), executor_ref.clone())
                .await
                .unwrap();
            run = service.snapshot().await.runs[&run_id].clone();
        }
        service
            .request_verification(run_id, run.generation)
            .await
            .unwrap();
        let phase_started_at = service.snapshot().await.runs[&run_id].phase_started_at;

        service
            .executor_event(
                run_id,
                run.generation,
                ExecutorEventKind::Started,
                Some(executor_ref),
                "replayed start acknowledgment".into(),
                false,
            )
            .await
            .unwrap();

        let after = service.snapshot().await;
        assert_eq!(after.runs[&run_id].state, RunState::Running);
        assert_eq!(after.runs[&run_id].phase, Phase::Verifying);
        assert_eq!(after.runs[&run_id].phase_started_at, phase_started_at);
        assert!(after.commands.values().any(|command| {
            command.run_id == run_id
                && command.kind == CommandKind::Verify
                && command.delivery_state == CommandDeliveryState::Pending
        }));
    }

    #[tokio::test]
    async fn hook_restart_observes_and_reverifies_without_replaying_dispatch() {
        let (_temp, config, service, marker) = hook_test_service().await;
        let (run_id, dispatch_id) = create_pending_dispatch(&service).await;
        service.process_pending_commands().await.unwrap();
        let started = service.snapshot().await;
        assert_eq!(started.runs[&run_id].state, RunState::Running);
        assert_eq!(started.runs[&run_id].phase, Phase::Deploying);
        assert_eq!(
            started.commands[&dispatch_id].delivery_state,
            CommandDeliveryState::Acknowledged
        );

        let run = started.runs[&run_id].clone();
        service
            .transact(Some("test-lane"), |engine, state, _| {
                engine.phase_grant(
                    state,
                    run_id,
                    run.generation,
                    run.executor_ref.as_deref().unwrap(),
                    Phase::Verifying,
                    Utc::now(),
                )?;
                let verify = engine.request_verification(
                    state,
                    run_id,
                    run.generation,
                    Uuid::new_v4(),
                    Utc::now(),
                )?;
                engine.command_delivery(
                    state,
                    verify.id,
                    CommandDelivery::Delivered,
                    Utc::now(),
                )?;
                Ok(())
            })
            .await
            .unwrap();

        drop(service);
        let reopened = Service::open(config).await.unwrap();
        reopened.scheduler_tick().await.unwrap();
        let after = reopened.snapshot().await;
        assert_eq!(after.runs[&run_id].state, RunState::Succeeded);
        assert!(
            after
                .receipts
                .contains_key(&after.runs[&run_id].receipt_id.unwrap())
        );
        let dispatches = std::fs::read_to_string(marker).unwrap();
        assert_eq!(
            dispatches.lines().count(),
            1,
            "dispatch must not replay after restart"
        );
        assert_eq!(
            ScriptedExecutor::synthetic_physical_start_count(&after),
            0,
            "hook execution must never be represented as synthetic"
        );
    }
    #[tokio::test]
    async fn lost_observation_after_ack_blocks_until_independent_evidence() {
        let (_temp, service) = test_service().await;
        let (run_id, command_id) = create_pending_dispatch(&service).await;
        service.process_pending_commands().await.unwrap();
        let run = service.snapshot().await.runs[&run_id].clone();
        service
            .executor_event(
                run_id,
                run.generation,
                ExecutorEventKind::Unknown,
                run.executor_ref.clone(),
                "observation lost after a known start".into(),
                false,
            )
            .await
            .unwrap();
        let unknown = service.snapshot().await;
        assert_eq!(unknown.runs[&run_id].state, RunState::Unknown);
        assert_eq!(unknown.lanes["test-lane"].active_run_id, Some(run_id));
        assert_eq!(
            unknown.commands[&command_id].delivery_state,
            CommandDeliveryState::Acknowledged
        );
        assert!(
            service
                .resolve_unknown(
                    "test-lane".into(),
                    ResolveAction::ConfirmStopped,
                    "human assertion".into(),
                    "no independent stop proof".into()
                )
                .await
                .is_err()
        );
        assert_eq!(
            service.snapshot().await.runs[&run_id].state,
            RunState::Unknown
        );
        service
            .resolve_unknown(
                "test-lane".into(),
                ResolveAction::AdoptRunning,
                "re-observe scripted identity".into(),
                "recover known synthetic execution".into(),
            )
            .await
            .unwrap();
        let restored = service.snapshot().await;
        assert_eq!(restored.runs[&run_id].state, RunState::Running);
        assert_eq!(
            ScriptedExecutor::synthetic_physical_start_count(&restored),
            1
        );
    }
    fn failed_observation(run: &crate::domain::DeliveryRun) -> HookReply {
        HookReply {
            version: 1,
            run_id: run.id,
            generation: run.generation,
            executor_ref: run.executor_ref.clone(),
            status: HookStatus::Failed,
            message: Some("deployment failed; inspect environment".into()),
            environment_known_unchanged: Some(false),
            logs: vec![],
            receipt: None,
            failure_proof: None,
        }
    }

    #[tokio::test]
    async fn repeated_failed_observations_are_durable_snapshots_not_log_output() {
        let (_temp, config, service, _) = hook_test_service().await;
        let (run_id, _) = create_pending_dispatch(&service).await;
        service.process_pending_commands().await.unwrap();
        let run = service.snapshot().await.runs[&run_id].clone();
        let mut reply = failed_observation(&run);
        reply.logs.push(crate::hooks::HookLog {
            sequence: 1,
            level: "error".into(),
            message: "same output".into(),
        });
        service
            .apply_hook_reply(HookOperation::Observe, None, &run, reply.clone())
            .await
            .unwrap();
        let first = service.snapshot().await;
        for _ in 0..50 {
            service
                .apply_hook_reply(HookOperation::Observe, None, &run, reply.clone())
                .await
                .unwrap();
        }
        let repeated = service.snapshot().await;
        assert_eq!(repeated.events, first.events);
        assert_eq!(
            repeated.runs[&run_id].updated_at,
            first.runs[&run_id].updated_at
        );
        assert_eq!(repeated.runs[&run_id].state, RunState::Unknown);
        assert_eq!(repeated.lanes["test-lane"].active_run_id, Some(run_id));
        drop(service);
        let service = Service::open(config).await.unwrap();
        service
            .apply_hook_reply(HookOperation::Observe, None, &run, reply.clone())
            .await
            .unwrap();
        assert_eq!(
            service.snapshot().await.events,
            first.events,
            "deduplication survives restart"
        );
        reply.logs.push(crate::hooks::HookLog {
            sequence: 2,
            level: "error".into(),
            message: "same output".into(),
        });
        service
            .apply_hook_reply(HookOperation::Observe, None, &run, reply.clone())
            .await
            .unwrap();
        let output = service.snapshot().await;
        assert_eq!(
            output.events.len(),
            first.events.len() + 1,
            "new sequence with identical text is real output"
        );
        assert!(
            output
                .events
                .last()
                .unwrap()
                .message
                .contains("[hook_seq:2]")
        );
        reply.message = Some("different failure detail".into());
        service
            .apply_hook_reply(HookOperation::Observe, None, &run, reply.clone())
            .await
            .unwrap();
        let changed = service.snapshot().await;
        assert_eq!(
            changed.events.len(),
            output.events.len() + 3,
            "changed reason, marker, summary"
        );
        service
            .apply_hook_reply(HookOperation::Observe, None, &run, reply)
            .await
            .unwrap();
        assert_eq!(service.snapshot().await.events, changed.events);
        service
            .mark_hook_operation_unknown(&run, None, "connection refused")
            .await
            .unwrap();
        let transport = service.snapshot().await;
        for _ in 0..10 {
            service
                .mark_hook_operation_unknown(&run, None, "connection refused")
                .await
                .unwrap();
        }
        assert_eq!(service.snapshot().await.events, transport.events);
    }

    fn write_failure_observer(path: &Path, mode: &str) {
        std::fs::write(path, format!(r#"
import datetime, json, sys
request = json.load(sys.stdin)
run = request['run']
mode = {mode:?}
reply = {{'version': 1, 'run_id': run['id'], 'generation': run['generation'],
         'executor_ref': run['executor_ref'], 'status': 'failed', 'message': 'failed supervisor'}}
if mode != 'unproven':
    reply['failure_proof'] = {{'observed_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
                             'evidence': {{'supervisor': 'stopped', 'cgroup': 'empty'}}}}
if mode == 'running':
    reply['status'] = 'running'
    reply.pop('failure_proof')
if mode == 'wrong_generation': reply['generation'] += 1
if mode == 'wrong_ref': reply['executor_ref'] = 'other-unit'
if mode == 'stale': reply['failure_proof']['observed_at'] = '2000-01-01T00:00:00Z'
print(json.dumps(reply))
"#)).unwrap();
    }

    #[tokio::test]
    async fn failure_only_observer_recovers_after_restart_with_explicit_retry_barrier() {
        let (_temp, mut config, service, marker) = hook_test_service().await;
        let (run_id, _) = create_pending_dispatch(&service).await;
        service.process_pending_commands().await.unwrap();
        let run = service.snapshot().await.runs[&run_id].clone();
        service
            .apply_hook_reply(HookOperation::Observe, None, &run, failed_observation(&run))
            .await
            .unwrap();
        let profile = config.effective_profile_digest(&config.lanes[0]).unwrap();
        let path = marker.with_file_name("failure-observe.py");
        write_failure_observer(&path, "failed");
        config.lanes[0].failure_observe = Some(HookConfig::Script {
            path,
            interpreter: "python3".into(),
            args: vec![],
            cwd: None,
            timeout_ms: 5_000,
        });
        assert_eq!(
            config.effective_profile_digest(&config.lanes[0]).unwrap(),
            profile
        );
        drop(service);
        let service = Service::open(config).await.unwrap();
        assert!(service.observe_failed_stop(&run).await.unwrap());
        let after = service.snapshot().await;
        assert_eq!(after.runs[&run_id].state, RunState::Failed);
        assert!(after.runs[&run_id].receipt_id.is_none());
        assert!(after.receipts.is_empty());
        assert_eq!(after.lanes["test-lane"].active_run_id, None);
        assert!(after.lanes["test-lane"].failure_paused);
        let request_id = *after.requests.keys().next().unwrap();
        assert_eq!(
            after.requests[&request_id].status,
            RequestStatus::NeedsAttention
        );
        assert!(
            after
                .events
                .iter()
                .any(|e| e.kind == "failure_stop_verified"
                    && e.message.contains("observer_config_sha256"))
        );
        service.scheduler_tick().await.unwrap();
        assert_eq!(
            service.snapshot().await.runs.len(),
            1,
            "failure must not automatically retry"
        );
        assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
        assert!(!service.observe_failed_stop(&run).await.unwrap());
        assert_eq!(service.snapshot().await.events, after.events);
        service.retry(request_id).await.unwrap();
        assert!(!service.snapshot().await.lanes["test-lane"].failure_paused);
        service.scheduler_tick().await.unwrap();
        let next = service.snapshot().await;
        let new_owner = next.lanes["test-lane"].active_run_id;
        assert!(new_owner.is_some() && new_owner != Some(run_id));
        assert!(!service.observe_failed_stop(&run).await.unwrap());
        assert_eq!(
            service.snapshot().await.lanes["test-lane"].active_run_id,
            new_owner
        );
        let stale = service
            .transact(Some("test-lane"), |engine, state, _| {
                engine.confirm_failed_proven(
                    state,
                    run_id,
                    run.generation + 1,
                    run.executor_ref.as_deref().unwrap(),
                    "proof",
                    "late proof",
                    Utc::now(),
                )?;
                Ok(())
            })
            .await;
        assert!(stale.is_err());
        assert_eq!(
            service.snapshot().await.lanes["test-lane"].active_run_id,
            new_owner
        );
    }

    #[tokio::test]
    async fn failure_only_observer_cannot_adopt_running_or_use_unproven_stop() {
        let (_temp, mut config, service, marker) = hook_test_service().await;
        let (run_id, _) = create_pending_dispatch(&service).await;
        service.process_pending_commands().await.unwrap();
        let run = service.snapshot().await.runs[&run_id].clone();
        service
            .apply_hook_reply(HookOperation::Observe, None, &run, failed_observation(&run))
            .await
            .unwrap();
        let path = marker.with_file_name("failure-observe.py");
        write_failure_observer(&path, "unproven");
        config.lanes[0].failure_observe = Some(HookConfig::Script {
            path: path.clone(),
            interpreter: "python3".into(),
            args: vec![],
            cwd: None,
            timeout_ms: 5_000,
        });
        drop(service);
        let service = Service::open(config).await.unwrap();
        let before = service.snapshot().await;
        for mode in [
            "running",
            "unproven",
            "wrong_generation",
            "wrong_ref",
            "stale",
        ] {
            write_failure_observer(&path, mode);
            assert!(!service.observe_failed_stop(&run).await.unwrap(), "{mode}");
            let after = service.snapshot().await;
            assert_eq!(after.runs[&run_id].state, RunState::Unknown, "{mode}");
            assert_eq!(
                after.lanes["test-lane"].active_run_id,
                Some(run_id),
                "{mode}"
            );
            assert_eq!(after.events, before.events, "{mode}");
        }
        assert!(
            service
                .resolve_unknown(
                    "test-lane".into(),
                    ResolveAction::ConfirmStopped,
                    "operator assertion".into(),
                    "no proof".into()
                )
                .await
                .is_err()
        );
        assert_eq!(
            service.snapshot().await.lanes["test-lane"].active_run_id,
            Some(run_id)
        );
    }
}
