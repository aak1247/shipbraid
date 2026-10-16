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
    config::{Config, ResolvedToken},
    domain::{
        Command, CommandDelivery, CommandDeliveryState, CommandKind, CoverageMode, DomainError,
        ExecutorResult, Lane, Phase, Priority, ReconcileIds, RequestStatus, RunState, State,
        SubmitInput, SubmitOutcome, Target, UnknownResolution,
    },
    engine::{Engine, EngineConfig},
    executor::{Executor, ScriptedExecutor},
    lineage::{CoverageContext, CoverageQuery, LineageResolver, is_full_revision},
    store::Store,
};

const MAX_PENDING_REQUESTS_PER_LANE: usize = 1_000;

#[derive(Debug, Error)]
pub enum ServiceError {
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
            max_manual_retries: 3,
        });
        let now = Utc::now();
        for lane_config in &config.lanes {
            let release_line_id = release_line_id(lane_config);
            let current = state.lanes.get(&lane_config.id);
            let mut lane = Lane {
                id: lane_config.id.clone(),
                project: lane_config.project.clone(),
                environment: lane_config.environment.clone(),
                enabled: lane_config.enabled,
                repository_id: lane_config.repository_id.clone(),
                release_ref: lane_config.release_ref.clone(),
                release_line_id,
                profile_digest: lane_config.profile_digest.clone(),
                config_version: current.map_or(1, |lane| lane.config_version),
                descendant_coverage_enabled: lane_config.allow_descendant,
                preemption_enabled: lane_config.allow_preemption,
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
            let unresolved_delivery = recovered.commands.values().any(|command| {
                command.run_id == run.id
                    && command.generation == run.generation
                    && matches!(command.kind, CommandKind::Dispatch | CommandKind::Cancel)
                    && matches!(
                        command.delivery_state,
                        CommandDeliveryState::Delivered | CommandDeliveryState::Unknown
                    )
            });
            if unresolved_delivery {
                engine.mark_unknown(
                    &mut recovered,
                    run.id,
                    run.generation,
                    "service restarted after command delivery without a durable operation result",
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
            _database_lock: database_lock,
            ready: AtomicBool::new(true),
        });
        Ok(service)
    }

    pub fn tokens(&self) -> &[ResolvedToken] {
        &self.tokens
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
        let _writer = self.writer.lock().await;
        let previous = self.state.read().await.clone();
        let lane = previous
            .lanes
            .get(&lane_id)
            .cloned()
            .ok_or_else(|| DomainError::LaneNotFound(lane_id.clone()))?;
        let target = Target {
            repository_id: lane.repository_id.clone(),
            revision: revision.clone(),
            components: components.into_iter().collect(),
            profile_digest: lane.profile_digest.clone(),
            release_line_id: lane.release_line_id.clone(),
            config_version: lane.config_version,
            coverage_mode,
        };
        let authorization = self.lineage.resolve_submission(
            &lane.repository_id,
            &lane.release_ref,
            &revision,
            &lane.release_line_id,
            lane.config_version,
        );
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
        if !is_full_revision(&target.revision) {
            return Err(DomainError::TargetNotAllowed(
                "revision must be a full immutable Git object ID".to_owned(),
            )
            .into());
        }
        let coverage = self.coverage_context(&previous, Some(&target), Some(&lane_id));
        let mut next = previous.clone();
        let result = self.engine.submit(
            &mut next,
            SubmitInput {
                id: Uuid::new_v4(),
                principal,
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
        self.commit_publish(&previous, next).await?;
        Ok(result)
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
                    let command = command.ok_or(DomainError::CommandNotFound)?;
                    engine.command_delivery(
                        state,
                        command.id,
                        CommandDelivery::Unknown {
                            reason: bounded_text(&reason, 2048),
                        },
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
            .filter(|run| run.state == RunState::Running)
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
            "another auto-deliver process already owns database {}",
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
                enabled: true,
            }],
            max_active_runs: 1,
            tick_ms: 1_000,
            scripted_phase_ms: 60_000,
            cancel_timeout_seconds: 30,
            coalesce_ms: 0,
        };
        let service = Service::open(config).await.unwrap();
        (temp, service)
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
}
