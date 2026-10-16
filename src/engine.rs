use std::{cmp::Reverse, collections::BTreeMap};

use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    domain::{
        Command, CommandDelivery, CommandDeliveryState, CommandKind, DeliveryRun,
        DeploymentReceipt, DomainError, Event, ExecutorResult, LOCAL_TENANT_ID, Lane, Phase,
        Priority, ReceiptResult, ReconcileIds, ReconcileOutcome, ReleaseRequest, RequestStatus,
        RunState, State, SubmitInput, SubmitOutcome, Target, TargetAuthorization,
        UnknownResolution,
    },
    lineage::{CoverageContext, CoverageQuery, is_full_revision},
};

const CONFIG_CHANGED_BLOCK: &str = "lane configuration changed while a run owns the lane";
const FAILURE_BLOCK: &str = "failure_paused";
const UNKNOWN_BLOCK: &str = "execution ownership is unknown";
const DEFAULT_MAX_ACTIVE_RUNS: usize = 2;
const DEFAULT_COALESCE_WINDOW: Duration = Duration::seconds(2);
const DEFAULT_MAX_MANUAL_RETRIES: u32 = 3;

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub max_active_runs: usize,
    pub coalesce_window: Duration,
    pub max_manual_retries: u32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_active_runs: DEFAULT_MAX_ACTIVE_RUNS,
            coalesce_window: DEFAULT_COALESCE_WINDOW,
            max_manual_retries: DEFAULT_MAX_MANUAL_RETRIES,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Engine {
    config: EngineConfig,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self { config }
    }

    pub fn register_lane(
        &self,
        state: &mut State,
        mut lane: Lane,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        if lane.tenant_id != LOCAL_TENANT_ID {
            return Err(DomainError::TargetNotAllowed(
                "only the local tenant is supported".to_owned(),
            ));
        }
        if lane.id.trim().is_empty()
            || lane.project.trim().is_empty()
            || lane.environment.trim().is_empty()
            || lane.repository_id.trim().is_empty()
            || lane.release_ref.trim().is_empty()
            || lane.release_line_id.trim().is_empty()
            || lane.profile_digest.trim().is_empty()
            || lane.allowed_components.is_empty()
        {
            return Err(DomainError::TargetNotAllowed(
                "lane configuration is incomplete".to_owned(),
            ));
        }

        let lane_id = lane.id.clone();
        let Some(existing) = state.lanes.get(&lane_id).cloned() else {
            lane.active_run_id = None;
            lane.generation = 0;
            lane.blocked_reason = None;
            lane.failure_paused = false;
            state.lanes.insert(lane_id.clone(), lane);
            emit(
                state,
                &lane_id,
                None,
                None,
                now,
                "lane_registered",
                "lane configuration registered",
            );
            return Ok(());
        };

        let changed = lane_config_changed(&existing, &lane);
        lane.active_run_id = existing.active_run_id;
        lane.generation = existing.generation;
        lane.failure_paused = existing.failure_paused;
        lane.blocked_reason = existing.blocked_reason.clone();

        if changed && lane.active_run_id.is_some() {
            if lane.blocked_reason.is_none() && !lane.failure_paused {
                lane.blocked_reason = Some(CONFIG_CHANGED_BLOCK.to_owned());
            }
        } else if lane.active_run_id.is_none()
            && lane.blocked_reason.as_deref() == Some(CONFIG_CHANGED_BLOCK)
            && !lane.failure_paused
        {
            lane.blocked_reason = None;
        }

        state.lanes.insert(lane_id.clone(), lane);
        if changed {
            emit(
                state,
                &lane_id,
                existing.active_run_id,
                None,
                now,
                "lane_config_updated",
                if existing.active_run_id.is_some() {
                    "desired lane config changed under active ownership; new dispatch is blocked"
                } else {
                    "lane configuration updated"
                },
            );
        }
        Ok(())
    }

    pub fn submit(
        &self,
        state: &mut State,
        input: SubmitInput,
        now: DateTime<Utc>,
        coverage: &CoverageContext,
    ) -> Result<SubmitOutcome, DomainError> {
        let lane = state
            .lanes
            .get(&input.lane_id)
            .cloned()
            .ok_or_else(|| DomainError::LaneNotFound(input.lane_id.clone()))?;

        if lane.tenant_id != LOCAL_TENANT_ID {
            return Err(DomainError::TargetNotAllowed(
                "only the local tenant is supported".to_owned(),
            ));
        }

        if input.principal.trim().is_empty()
            || input.idempotency_key.trim().is_empty()
            || input.request_hash.trim().is_empty()
        {
            return Err(DomainError::TargetNotAllowed(
                "principal, idempotency key, and request hash are required".to_owned(),
            ));
        }

        if let Some(existing) = state.requests.values().find(|request| {
            request.principal == input.principal
                && request.idempotency_key == input.idempotency_key
                && state
                    .lanes
                    .get(&request.lane_id)
                    .is_some_and(|request_lane| request_lane.project == lane.project)
        }) {
            if existing.request_hash != input.request_hash {
                return Err(DomainError::IdempotencyConflict);
            }
            return Ok(SubmitOutcome {
                request_id: existing.id,
                reused: true,
                attached_run_id: existing.current_run_id,
            });
        }
        if state.requests.contains_key(&input.id) {
            return Err(DomainError::DuplicateId);
        }
        if input.deadline_at.is_some_and(|deadline| deadline <= now) {
            return Err(DomainError::DeadlineElapsed);
        }
        validate_target(&lane, &input.target, &input.authorization)?;

        let mut request = ReleaseRequest {
            tenant_id: lane.tenant_id.clone(),
            context: input.context,
            id: input.id,
            lane_id: lane.id.clone(),
            principal: input.principal,
            idempotency_key: input.idempotency_key,
            request_hash: input.request_hash,
            target: input.target,
            priority: input.priority,
            deadline_at: input.deadline_at,
            status: RequestStatus::Waiting,
            current_run_id: None,
            satisfied_by_run: None,
            satisfied_by_receipt: None,
            reason: input.reason,
            waiting_reason: None,
            created_at: now,
            creation_sequence: 0,
            auto_retries_used: 0,
            manual_retries_used: 0,
        };
        request.creation_sequence = emit(
            state,
            &lane.id,
            None,
            Some(request.id),
            now,
            "request_accepted",
            "release request accepted and persisted",
        );
        state.requests.insert(request.id, request.clone());

        let attached_run_id = attach_to_covering_active_run(state, &lane, &request, now, coverage);
        Ok(SubmitOutcome {
            request_id: request.id,
            reused: false,
            attached_run_id,
        })
    }

    pub fn set_priority(
        &self,
        state: &mut State,
        request_id: Uuid,
        priority: Priority,
        reason: String,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let mut request = state
            .requests
            .get(&request_id)
            .cloned()
            .ok_or(DomainError::RequestNotFound)?;
        if request.status.is_terminal() {
            return Err(DomainError::RequestTerminal);
        }
        if priority < request.priority {
            return Err(DomainError::InvalidTransition);
        }
        if priority == request.priority {
            return Ok(());
        }
        request.priority = priority;
        state.requests.insert(request_id, request.clone());
        let effective_priority = request
            .current_run_id
            .map(|run_id| run_effective_priority(state, run_id));
        if let (Some(run_id), Some(priority)) = (request.current_run_id, effective_priority)
            && let Some(run) = state.runs.get_mut(&run_id)
        {
            run.effective_priority = priority;
        }
        emit(
            state,
            &request.lane_id,
            request.current_run_id,
            Some(request_id),
            now,
            "request_priority_raised",
            &format!("request priority raised to urgent: {}", reason.trim()),
        );
        Ok(())
    }

    pub fn reconcile(
        &self,
        state: &mut State,
        lane_id: &str,
        now: DateTime<Utc>,
        ids: ReconcileIds,
        coverage: &CoverageContext,
    ) -> Result<ReconcileOutcome, DomainError> {
        self.expire(state, now);
        let lane = state
            .lanes
            .get(lane_id)
            .cloned()
            .ok_or_else(|| DomainError::LaneNotFound(lane_id.to_owned()))?;

        if let Some(active_run_id) = lane.active_run_id {
            let Some(mut active) = state.runs.get(&active_run_id).cloned() else {
                return Ok(ReconcileOutcome::Blocked {
                    reason: "active_run_record_missing".to_owned(),
                });
            };
            if !active.state.owns_execution_slot() {
                return Ok(ReconcileOutcome::Blocked {
                    reason: "active_run_state_does_not_own_slot".to_owned(),
                });
            }
            if active.generation != lane.generation {
                return Ok(ReconcileOutcome::Blocked {
                    reason: "active_run_generation_mismatch".to_owned(),
                });
            }

            attach_covered_waiters(state, &lane, &mut active, now, coverage);
            let live_waiters = request_ids_for_run(state, active.id);
            if live_waiters.is_empty()
                && active.phase.is_preemptible()
                && matches!(active.state, RunState::Running)
                && active.replacement_run_id.is_none()
            {
                return self.cancel_unneeded_run(state, &lane, active, ids.command_id, now);
            }

            if !lane.enabled {
                return Ok(ReconcileOutcome::Blocked {
                    reason: "lane_disabled".to_owned(),
                });
            }
            if active.state == RunState::Unknown {
                return Ok(ReconcileOutcome::Blocked {
                    reason: lane
                        .blocked_reason
                        .unwrap_or_else(|| UNKNOWN_BLOCK.to_owned()),
                });
            }
            if active.state == RunState::CancelRequested {
                return Ok(ReconcileOutcome::Blocked {
                    reason: "stop_confirmation_pending".to_owned(),
                });
            }
            if lane.failure_paused || lane.blocked_reason.is_some() {
                return Ok(ReconcileOutcome::Blocked {
                    reason: lane
                        .blocked_reason
                        .unwrap_or_else(|| FAILURE_BLOCK.to_owned()),
                });
            }

            if !state.draining
                && active.state == RunState::Running
                && active.phase.is_preemptible()
                && lane.preemption_enabled
                && active.effective_priority == Priority::Normal
                && !live_waiters.is_empty()
                && let Some(candidate) =
                    self.preemption_candidate(state, &lane, &live_waiters, coverage)
            {
                return self.request_preemption(
                    state,
                    &lane,
                    active,
                    &candidate,
                    ids.run_id,
                    ids.command_id,
                    now,
                );
            }
            return Ok(ReconcileOutcome::Idle);
        }

        if let Some((run_id, _)) = state
            .runs
            .iter()
            .find(|(_, run)| run.lane_id == lane_id && run.state.owns_execution_slot())
        {
            return Ok(ReconcileOutcome::Blocked {
                reason: format!("orphaned_active_run:{run_id}"),
            });
        }
        mark_stale_lane_requests(state, &lane, now);
        if !lane.enabled {
            set_waiting_reason(state, lane_id, "lane_disabled");
            return Ok(ReconcileOutcome::Blocked {
                reason: "lane_disabled".to_owned(),
            });
        }
        if lane.failure_paused || lane.blocked_reason.is_some() {
            let reason = lane
                .blocked_reason
                .clone()
                .unwrap_or_else(|| FAILURE_BLOCK.to_owned());
            set_waiting_reason(state, lane_id, &reason);
            return Ok(ReconcileOutcome::Blocked { reason });
        }
        if state.draining {
            set_waiting_reason(state, lane_id, "service_draining");
            return Ok(ReconcileOutcome::Blocked {
                reason: "service_draining".to_owned(),
            });
        }

        let candidates = state
            .requests
            .values()
            .filter(|request| {
                request.lane_id == lane_id
                    && request.current_run_id.is_none()
                    && matches!(
                        request.status,
                        RequestStatus::Accepted | RequestStatus::Waiting
                    )
                    && request.deadline_at.is_none_or(|deadline| deadline > now)
            })
            .cloned()
            .collect::<Vec<_>>();
        let Some(batch) = choose_candidate_batch(&candidates, &lane, coverage) else {
            return Ok(ReconcileOutcome::Idle);
        };
        let candidate = batch.candidate.clone();
        let anchor = batch.anchor.clone();

        if batch.effective_priority == Priority::Normal && !self.config.coalesce_window.is_zero() {
            let until = anchor.created_at + self.config.coalesce_window;
            if now < until {
                for request in &batch.requests {
                    if let Some(current) = state.requests.get_mut(&request.id) {
                        current.waiting_reason = Some("coalescing_window".to_owned());
                    }
                }
                return Ok(ReconcileOutcome::WaitingForMergeWindow {
                    request_id: anchor.id,
                    until,
                });
            }
        }

        let active_count = state
            .runs
            .values()
            .filter(|run| run.state.owns_execution_slot())
            .count();
        if active_count >= self.config.max_active_runs {
            set_waiting_reason(state, lane_id, "global_permit_unavailable");
            return Ok(ReconcileOutcome::PermitUnavailable);
        }
        if state.runs.contains_key(&ids.run_id) || state.commands.contains_key(&ids.command_id) {
            return Err(DomainError::DuplicateId);
        }

        let attached = batch
            .requests
            .iter()
            .map(|request| request.id)
            .collect::<Vec<_>>();
        if attached.is_empty() {
            return Err(DomainError::TargetNotAllowed(
                "selected candidate does not cover its own request".to_owned(),
            ));
        }

        let generation = lane.generation + 1;
        let effective_priority = batch.effective_priority;
        let run = DeliveryRun {
            id: ids.run_id,
            lane_id: lane.id.clone(),
            target: candidate.target.clone(),
            effective_priority,
            state: RunState::Dispatching,
            phase: Phase::Preparing,
            generation,
            executor_ref: None,
            replacement_run_id: None,
            cancel_reason: None,
            receipt_id: None,
            created_at: now,
            phase_started_at: now,
            updated_at: now,
        };
        let command = new_command(
            ids.command_id,
            run.id,
            generation,
            CommandKind::Dispatch,
            now,
            &format!(
                "dispatch:{}:{}:{}",
                run.id, run.generation, run.target.revision
            ),
        );
        for request_id in &attached {
            if let Some(request) = state.requests.get_mut(request_id) {
                request.current_run_id = Some(run.id);
                request.status = RequestStatus::Waiting;
                request.waiting_reason = None;
            }
        }
        state.runs.insert(run.id, run.clone());
        state.commands.insert(command.id, command.clone());
        if let Some(lane) = state.lanes.get_mut(lane_id) {
            lane.generation = generation;
            lane.active_run_id = Some(run.id);
            lane.blocked_reason = None;
        }
        emit(
            state,
            lane_id,
            Some(run.id),
            None,
            now,
            "run_dispatch_requested",
            &format!(
                "created run generation {generation} for {} request(s)",
                attached.len()
            ),
        );
        for request_id in &attached {
            emit(
                state,
                lane_id,
                Some(run.id),
                Some(*request_id),
                now,
                "request_bound_to_run",
                &format!("request bound to run {} during dispatch", run.id),
            );
        }
        Ok(ReconcileOutcome::Dispatched {
            run_id: run.id,
            command_id: command.id,
            attached_requests: attached,
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Transitions carry explicit state, ownership fence, proof, and clock inputs"
    )]
    pub fn confirm_stopped(
        &self,
        state: &mut State,
        run_id: Uuid,
        generation: u64,
        executor_ref: &str,
        command_id: Uuid,
        now: DateTime<Utc>,
        coverage: &CoverageContext,
    ) -> Result<(), DomainError> {
        if executor_ref.trim().is_empty() {
            return Err(DomainError::OwnershipUnproven);
        }
        self.confirm_stopped_proven(
            state,
            run_id,
            generation,
            Some(executor_ref),
            command_id,
            now,
            coverage,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Transitions carry explicit state, ownership fence, proof, and clock inputs"
    )]
    fn confirm_stopped_proven(
        &self,
        state: &mut State,
        run_id: Uuid,
        generation: u64,
        executor_ref: Option<&str>,
        command_id: Uuid,
        now: DateTime<Utc>,
        coverage: &CoverageContext,
    ) -> Result<(), DomainError> {
        let mut old = state
            .runs
            .get(&run_id)
            .cloned()
            .ok_or(DomainError::RunNotFound)?;
        let mut lane = state
            .lanes
            .get(&old.lane_id)
            .cloned()
            .ok_or_else(|| DomainError::LaneNotFound(old.lane_id.clone()))?;
        if old.generation != generation
            || lane.active_run_id != Some(run_id)
            || lane.generation != generation
        {
            if matches!(old.state, RunState::Canceled | RunState::Superseded)
                && old.generation == generation
                && executor_ref.is_none_or(|supplied| old.executor_ref.as_deref() == Some(supplied))
                && state.events.iter().any(|event| {
                    event.run_id == Some(run_id)
                        && matches!(event.kind.as_str(), "run_canceled" | "run_superseded")
                })
            {
                return Ok(());
            }
            return Err(DomainError::StaleGeneration);
        }
        if !matches!(old.state, RunState::CancelRequested | RunState::Unknown) {
            return Err(DomainError::InvalidTransition);
        }
        if old
            .executor_ref
            .as_deref()
            .is_some_and(|current| executor_ref.is_some_and(|supplied| current != supplied))
        {
            return Err(DomainError::ExecutorMismatch);
        }
        if state.commands.contains_key(&command_id) {
            return Err(DomainError::DuplicateId);
        }

        self.expire(state, now);
        let successor_id = old.replacement_run_id;
        let successor = successor_id.and_then(|id| state.runs.get(&id).cloned());
        let next_generation = generation + 1;
        if let Some(executor_ref) = executor_ref {
            old.executor_ref = Some(executor_ref.to_owned());
        }
        old.updated_at = now;

        let mut attached = Vec::new();
        let mut can_start_successor = false;
        let successor_policy_allows_dispatch = lane.enabled && !state.draining;
        if let Some(successor) = successor.as_ref() {
            let requests = state
                .requests
                .values()
                .filter(|request| {
                    request.lane_id == lane.id
                        && !request.status.is_terminal()
                        && (request.current_run_id == Some(run_id)
                            || request.current_run_id.is_none())
                        && matches!(
                            request.status,
                            RequestStatus::Accepted | RequestStatus::Waiting
                        )
                })
                .cloned()
                .collect::<Vec<_>>();
            can_start_successor = successor_policy_allows_dispatch
                && successor.state == RunState::Planned
                && successor.generation == next_generation
                && target_matches_lane(&lane, &successor.target)
                && requests
                    .iter()
                    .any(|request| request.target == successor.target);
            if can_start_successor {
                for request in &requests {
                    if target_covers(
                        &successor.target,
                        &request.target,
                        &lane.id,
                        &request.lane_id,
                        coverage,
                    )
                    .is_some()
                    {
                        attached.push(request.id);
                    }
                }
            }
        }

        if can_start_successor {
            let mut successor = successor.expect("validated successor exists");
            old.state = RunState::Superseded;
            successor.state = RunState::Dispatching;
            successor.phase_started_at = now;
            successor.updated_at = now;
            successor.effective_priority = attached
                .iter()
                .filter_map(|id| state.requests.get(id).map(|request| request.priority))
                .max()
                .unwrap_or(Priority::Urgent);
            let command = new_command(
                command_id,
                successor.id,
                next_generation,
                CommandKind::Dispatch,
                now,
                &format!(
                    "dispatch:{}:{}:{}",
                    successor.id, next_generation, successor.target.revision
                ),
            );
            for request_id in &attached {
                if let Some(request) = state.requests.get_mut(request_id) {
                    request.current_run_id = Some(successor.id);
                    request.status = RequestStatus::Waiting;
                    request.waiting_reason = None;
                }
            }
            state.runs.insert(successor.id, successor.clone());
            state.commands.insert(command.id, command);
            lane.generation = next_generation;
            lane.active_run_id = Some(successor.id);
            lane.blocked_reason = None;
            lane.failure_paused = false;
            emit(
                state,
                &lane.id,
                Some(old.id),
                None,
                now,
                "run_superseded",
                &format!(
                    "trusted stop confirmed; ownership transferred to {}",
                    successor.id
                ),
            );
            emit(
                state,
                &lane.id,
                Some(successor.id),
                None,
                now,
                "replacement_dispatch_requested",
                &format!(
                    "replacement run {} dispatched after previous run {} with {} waiter(s)",
                    successor.id,
                    old.id,
                    attached.len(),
                ),
            );
            for request_id in &attached {
                emit(
                    state,
                    &lane.id,
                    Some(successor.id),
                    Some(*request_id),
                    now,
                    "request_transferred_to_run",
                    &format!(
                        "request transferred from previous run {} to replacement run {}",
                        old.id, successor.id
                    ),
                );
            }
        } else {
            old.state = RunState::Canceled;
            if let Some(successor_id) = successor_id
                && let Some(successor) = state.runs.get_mut(&successor_id)
            {
                successor.state = RunState::Canceled;
                successor.updated_at = now;
            }
            for request in state.requests.values_mut().filter(|request| {
                request.current_run_id == Some(run_id) && !request.status.is_terminal()
            }) {
                request.current_run_id = None;
                request.waiting_reason = Some("previous_run_stopped".to_owned());
            }
            lane.generation = next_generation;
            lane.active_run_id = None;
            if lane.blocked_reason.as_deref() == Some(CONFIG_CHANGED_BLOCK) && !lane.failure_paused
            {
                lane.blocked_reason = None;
            }
            if is_unknown_block(lane.blocked_reason.as_deref()) && !lane.failure_paused {
                lane.blocked_reason = None;
            }
            emit(
                state,
                &lane.id,
                Some(old.id),
                None,
                now,
                "run_canceled",
                "trusted stop confirmed; no valid replacement batch remains",
            );
        }
        state.runs.insert(old.id, old);
        state.lanes.insert(lane.id.clone(), lane);
        Ok(())
    }

    pub fn phase_grant(
        &self,
        state: &mut State,
        run_id: Uuid,
        generation: u64,
        executor_ref: &str,
        next_phase: Phase,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let run = state
            .runs
            .get(&run_id)
            .cloned()
            .ok_or(DomainError::RunNotFound)?;
        ensure_active_owner(state, &run, generation)?;
        if run.state != RunState::Running {
            return Err(DomainError::InvalidTransition);
        }
        if run.executor_ref.as_deref() != Some(executor_ref) {
            return Err(DomainError::ExecutorMismatch);
        }
        let valid_next = matches!(
            (&run.phase, &next_phase),
            (Phase::Preparing, Phase::Building)
                | (Phase::Building, Phase::Deploying)
                | (Phase::Deploying, Phase::Verifying)
        );
        if !valid_next {
            if run.phase == next_phase {
                return Ok(());
            }
            return Err(DomainError::InvalidTransition);
        }
        if let Some(stored) = state.runs.get_mut(&run_id) {
            stored.phase = next_phase.clone();
            stored.phase_started_at = now;
            stored.updated_at = now;
        }
        emit(
            state,
            &run.lane_id,
            Some(run_id),
            None,
            now,
            "phase_granted",
            &format!("executor received grant for {next_phase:?}"),
        );
        Ok(())
    }

    /// Fence an opaque external deployment hook as non-preemptible before any
    /// hook side effect begins. The caller must commit this transition before
    /// invoking the hook. Replays are a no-op while the same generation is
    /// still dispatching in Deploying; ownership or generation changes reject.
    pub fn begin_external_dispatch(
        &self,
        state: &mut State,
        run_id: Uuid,
        generation: u64,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let mut run = state
            .runs
            .get(&run_id)
            .cloned()
            .ok_or(DomainError::RunNotFound)?;
        ensure_active_owner(state, &run, generation)?;
        if run.state != RunState::Dispatching {
            return Err(DomainError::InvalidTransition);
        }
        if run.phase == Phase::Deploying {
            return Ok(());
        }
        if !matches!(&run.phase, Phase::Preparing | Phase::Building) {
            return Err(DomainError::InvalidTransition);
        }

        run.phase = Phase::Deploying;
        run.phase_started_at = now;
        run.updated_at = now;
        state.runs.insert(run_id, run.clone());
        emit(
            state,
            &run.lane_id,
            Some(run_id),
            None,
            now,
            "external_dispatch_admitted",
            "run entered the non-preemptible deployment phase before external hook side effects",
        );
        Ok(())
    }

    /// Produce the exact candidate/request Git pairs that coverage decisions
    /// for this lane can consult. The caller resolves these before invoking
    /// state transitions and shares the resulting context across them.
    pub fn coverage_queries(&self, state: &State, lane_id: &str) -> Vec<CoverageQuery> {
        let mut candidates_by_target = BTreeMap::<_, &Target>::new();
        for request in state
            .requests
            .values()
            .filter(|request| request.lane_id == lane_id && !request.status.is_terminal())
        {
            candidates_by_target
                .entry(candidate_target_key(&request.target))
                .or_insert(&request.target);
        }
        if let Some(run_id) = state.lanes.get(lane_id).and_then(|lane| lane.active_run_id)
            && let Some(run) = state.runs.get(&run_id)
        {
            candidates_by_target
                .entry(candidate_target_key(&run.target))
                .or_insert(&run.target);
        }

        let mut requested_by_target = BTreeMap::new();
        for request in state.requests.values().filter(|request| {
            request.lane_id == lane_id
                && !request.status.is_terminal()
                && request.target.coverage_mode == crate::domain::CoverageMode::Descendant
        }) {
            requested_by_target
                .entry(candidate_target_key(&request.target))
                .or_insert(&request.target);
        }
        let mut queries = std::collections::BTreeSet::new();
        for candidate in candidates_by_target.values() {
            for target in requested_by_target.values() {
                if candidate.revision == target.revision
                    || candidate.repository_id != target.repository_id
                    || candidate.release_line_id != target.release_line_id
                    || candidate.config_version != target.config_version
                    || candidate.profile_digest != target.profile_digest
                    || !target.components.is_subset(&candidate.components)
                    || !is_full_revision(&candidate.revision)
                    || !is_full_revision(&target.revision)
                {
                    continue;
                }
                queries.insert(CoverageQuery {
                    repository_id: candidate.repository_id.clone(),
                    candidate_revision: candidate.revision.clone(),
                    requested_revision: target.revision.clone(),
                    release_line_id: candidate.release_line_id.clone(),
                    config_version: candidate.config_version,
                });
            }
        }
        queries.into_iter().collect()
    }

    /// Create the durable verification command once a run reaches Verifying.
    /// Repeated scheduler ticks return the existing command instead of
    /// allocating another executor side effect.
    pub fn request_verification(
        &self,
        state: &mut State,
        run_id: Uuid,
        generation: u64,
        command_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Command, DomainError> {
        let run = state
            .runs
            .get(&run_id)
            .cloned()
            .ok_or(DomainError::RunNotFound)?;
        ensure_active_owner(state, &run, generation)?;
        if run.state != RunState::Running || run.phase != Phase::Verifying {
            return Err(DomainError::InvalidTransition);
        }
        if let Some(existing) = state.commands.values().find(|command| {
            command.run_id == run_id
                && command.generation == generation
                && command.kind == CommandKind::Verify
        }) {
            return Ok(existing.clone());
        }
        if state.commands.contains_key(&command_id) {
            return Err(DomainError::DuplicateId);
        }
        let command = new_command(
            command_id,
            run_id,
            generation,
            CommandKind::Verify,
            now,
            &format!(
                "verify:{}:{}:{}",
                run.id, run.generation, run.target.revision
            ),
        );
        state.commands.insert(command.id, command.clone());
        emit(
            state,
            &run.lane_id,
            Some(run.id),
            None,
            now,
            "verification_requested",
            "durable verification command created for the active generation",
        );
        Ok(command)
    }

    /// Persist a sanitized executor log line as an auditable event. Logs do
    /// not change run state and late lines remain useful after settlement.
    pub fn record_executor_log(
        &self,
        state: &mut State,
        run_id: Uuid,
        level: &str,
        message: &str,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let lane_id = state
            .runs
            .get(&run_id)
            .map(|run| run.lane_id.clone())
            .ok_or(DomainError::RunNotFound)?;
        let level = match level.trim().to_ascii_lowercase().as_str() {
            "debug" | "info" | "warn" | "error" => level.trim().to_ascii_lowercase(),
            _ => "info".to_owned(),
        };
        let sanitized = message
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .take(2_000)
            .collect::<String>();
        if sanitized.trim().is_empty() {
            return Err(DomainError::TargetNotAllowed(
                "executor log message is empty".to_owned(),
            ));
        }
        emit(
            state,
            &lane_id,
            Some(run_id),
            None,
            now,
            "executor_log",
            &format!("[{level}] {sanitized}"),
        );
        Ok(())
    }

    /// An observed process stop proves failure, never successful deployment or
    /// cancellation. Keep requests on an explicit-retry failure barrier.
    #[allow(clippy::too_many_arguments)]
    pub fn confirm_failed_proven(
        &self,
        state: &mut State,
        run_id: Uuid,
        generation: u64,
        executor_ref: &str,
        evidence_record: &str,
        message: &str,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        if evidence_record.trim().is_empty() || evidence_record.len() > 16_384 {
            return Err(DomainError::OwnershipUnproven);
        }
        let mut run = state
            .runs
            .get(&run_id)
            .cloned()
            .ok_or(DomainError::RunNotFound)?;
        if run.generation != generation
            || executor_ref.trim().is_empty()
            || run
                .executor_ref
                .as_deref()
                .is_some_and(|value| value != executor_ref)
        {
            return Err(DomainError::ExecutorMismatch);
        }
        if run.state == RunState::Failed {
            return Ok(());
        }
        ensure_active_owner(state, &run, generation)?;
        if !matches!(
            run.state,
            RunState::Dispatching
                | RunState::Running
                | RunState::CancelRequested
                | RunState::Unknown
        ) {
            return Err(DomainError::InvalidTransition);
        }
        run.executor_ref = Some(executor_ref.to_owned());
        run.state = RunState::Failed;
        run.updated_at = now;
        state.runs.insert(run_id, run.clone());
        emit(
            state,
            &run.lane_id,
            Some(run_id),
            None,
            now,
            "failure_stop_verified",
            evidence_record,
        );
        settle_failed_run(state, &run, now, message);
        Ok(())
    }

    /// Observation summaries are state snapshots, unlike sequenced executor
    /// output. Persist changed summaries while preserving every new log line.
    #[allow(clippy::too_many_arguments)]
    pub fn record_hook_observation(
        &self,
        state: &mut State,
        run_id: Uuid,
        source: &str,
        signature: &str,
        level: &str,
        message: &str,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let run = state.runs.get(&run_id).ok_or(DomainError::RunNotFound)?;
        let lane_id = run.lane_id.clone();
        let kind = format!("hook_observation.{source}");
        if state
            .events
            .iter()
            .rev()
            .find(|event| event.run_id == Some(run_id) && event.kind == kind)
            .is_some_and(|event| event.message == signature)
        {
            return Ok(());
        }
        emit(state, &lane_id, Some(run_id), None, now, &kind, signature);
        if !message.trim().is_empty() {
            self.record_executor_log(state, run_id, level, message, now)?;
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Transitions carry explicit state, ownership fence, proof, and clock inputs"
    )]
    pub fn executor_result(
        &self,
        state: &mut State,
        run_id: Uuid,
        generation: u64,
        executor_ref: &str,
        result: ExecutorResult,
        now: DateTime<Utc>,
        coverage: &CoverageContext,
    ) -> Result<(), DomainError> {
        let mut run = state
            .runs
            .get(&run_id)
            .cloned()
            .ok_or(DomainError::RunNotFound)?;
        if let ExecutorResult::VerifiedReceipt(receipt) = &result
            && let Some(previous) = state.receipts.get(&receipt.id)
        {
            if previous != receipt
                || receipt.run_id != run_id
                || receipt.generation != generation
                || run.generation != generation
                || run
                    .executor_ref
                    .as_deref()
                    .is_some_and(|stored| stored != executor_ref)
                || executor_ref.trim().is_empty()
            {
                return Err(DomainError::InvalidReceipt);
            }
            return Ok(());
        }
        ensure_active_owner(state, &run, generation)?;
        if run
            .executor_ref
            .as_deref()
            .is_some_and(|current| current != executor_ref)
        {
            return Err(DomainError::ExecutorMismatch);
        }
        if executor_ref.trim().is_empty() {
            return Err(DomainError::ExecutorMismatch);
        }

        match result {
            ExecutorResult::Started {
                executor_ref: received,
            } => {
                if received != executor_ref {
                    return Err(DomainError::ExecutorMismatch);
                }
                if run.state == RunState::Running
                    && run.executor_ref.as_deref() == Some(executor_ref)
                {
                    return Ok(());
                }
                if run.state != RunState::Dispatching {
                    return Err(DomainError::InvalidTransition);
                }
                run.executor_ref = Some(received);
                run.state = RunState::Running;
                if run.phase != Phase::Deploying {
                    run.phase_started_at = now;
                }
                run.updated_at = now;
                state.runs.insert(run_id, run.clone());
                clear_unknown_block(state, &run.lane_id);
                emit(
                    state,
                    &run.lane_id,
                    Some(run_id),
                    None,
                    now,
                    "run_started",
                    "executor confirmed the run started",
                );
            }
            ExecutorResult::Stopped { .. } => return Err(DomainError::InvalidTransition),
            ExecutorResult::Unknown { reason } => {
                mark_unknown_inner(state, &mut run, reason.trim(), now)?;
            }
            ExecutorResult::Failed {
                message,
                environment_known_unchanged,
            } => {
                if run.state == RunState::CancelRequested {
                    mark_unknown_inner(
                        state,
                        &mut run,
                        "failure arrived while awaiting an explicit stop confirmation",
                        now,
                    )?;
                } else if environment_known_unchanged {
                    run.state = RunState::Failed;
                    run.updated_at = now;
                    state.runs.insert(run_id, run.clone());
                    settle_failed_run(state, &run, now, &message);
                } else {
                    mark_unknown_inner(state, &mut run, &message, now)?;
                }
            }
            ExecutorResult::VerifiedReceipt(receipt) => {
                self.accept_receipt(state, &run, receipt, now, coverage)?;
            }
        }
        Ok(())
    }

    pub fn command_delivery(
        &self,
        state: &mut State,
        command_id: Uuid,
        delivery: CommandDelivery,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let mut command = state
            .commands
            .get(&command_id)
            .cloned()
            .ok_or(DomainError::CommandNotFound)?;
        if command.delivery_state == CommandDeliveryState::Acknowledged {
            return Ok(());
        }
        let run = state
            .runs
            .get(&command.run_id)
            .cloned()
            .ok_or(DomainError::RunNotFound)?;
        let Some(lane) = state.lanes.get(&run.lane_id).cloned() else {
            return Err(DomainError::LaneNotFound(run.lane_id.clone()));
        };
        match delivery {
            CommandDelivery::Delivered => {
                if command.delivery_state != CommandDeliveryState::Unknown {
                    command.delivery_state = CommandDeliveryState::Delivered;
                }
                state.commands.insert(command_id, command.clone());
                emit(
                    state,
                    &run.lane_id,
                    Some(run.id),
                    None,
                    now,
                    "command_delivered",
                    &format!("{:?} command delivered to executor", command.kind),
                );
            }
            CommandDelivery::Acknowledged => {
                // Transport acknowledgment records receipt only. It cannot
                // release lane ownership or prove a cancellation completed.
                command.delivery_state = CommandDeliveryState::Acknowledged;
                state.commands.insert(command_id, command);
                emit(
                    state,
                    &run.lane_id,
                    Some(run.id),
                    None,
                    now,
                    "command_acknowledged",
                    "executor acknowledged command receipt; operation result is still required",
                );
            }
            CommandDelivery::Unknown { reason } => {
                command.delivery_state = CommandDeliveryState::Unknown;
                state.commands.insert(command_id, command);
                if lane.active_run_id == Some(run.id) && lane.generation == run.generation {
                    let mut active = run.clone();
                    mark_unknown_inner(state, &mut active, &reason, now)?;
                } else {
                    emit(
                        state,
                        &run.lane_id,
                        Some(run.id),
                        None,
                        now,
                        "stale_command_delivery_unknown",
                        "stale command delivery is recorded without changing current lane ownership",
                    );
                }
            }
        }
        Ok(())
    }

    pub fn withdraw(
        &self,
        state: &mut State,
        request_id: Uuid,
        principal: &str,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let mut request = state
            .requests
            .get(&request_id)
            .cloned()
            .ok_or(DomainError::RequestNotFound)?;
        if request.principal != principal {
            return Err(DomainError::RequestNotOwned);
        }
        if request.status == RequestStatus::Withdrawn {
            return Ok(());
        }
        if request.status.is_terminal() {
            return Err(DomainError::RequestTerminal);
        }
        request.status = RequestStatus::Withdrawn;
        request.waiting_reason = None;
        request.current_run_id = None;
        state.requests.insert(request_id, request.clone());
        emit(
            state,
            &request.lane_id,
            None,
            Some(request_id),
            now,
            "request_withdrawn",
            "request withdrawn; other waiters and run ownership are unchanged",
        );
        Ok(())
    }

    pub fn retry(
        &self,
        state: &mut State,
        request_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let mut request = state
            .requests
            .get(&request_id)
            .cloned()
            .ok_or(DomainError::RequestNotFound)?;
        if request.status != RequestStatus::NeedsAttention {
            return Err(DomainError::NotRetryable);
        }
        if request.deadline_at.is_some_and(|deadline| deadline <= now) {
            return Err(DomainError::DeadlineElapsed);
        }
        if request.manual_retries_used >= self.config.max_manual_retries {
            return Err(DomainError::RetryLimitExceeded);
        }
        request.manual_retries_used += 1;
        request.status = RequestStatus::Waiting;
        request.current_run_id = None;
        request.waiting_reason = Some("manual_retry_requested".to_owned());
        state.requests.insert(request_id, request.clone());
        if let Some(lane) = state.lanes.get_mut(&request.lane_id)
            && lane.active_run_id.is_none()
            && lane.failure_paused
        {
            lane.failure_paused = false;
            lane.blocked_reason = None;
        }
        emit(
            state,
            &request.lane_id,
            None,
            Some(request_id),
            now,
            "request_retried",
            "explicit retry recorded; original target and deadline retained",
        );
        Ok(())
    }

    pub fn expire(&self, state: &mut State, now: DateTime<Utc>) {
        let expired = state
            .requests
            .values()
            .filter(|request| {
                !request.status.is_terminal()
                    && request.deadline_at.is_some_and(|deadline| deadline <= now)
            })
            .map(|request| request.id)
            .collect::<Vec<_>>();
        for request_id in expired {
            if let Some(request) = state.requests.get_mut(&request_id) {
                request.status = RequestStatus::Expired;
                request.current_run_id = None;
                request.waiting_reason = None;
                let lane_id = request.lane_id.clone();
                emit(
                    state,
                    &lane_id,
                    None,
                    Some(request_id),
                    now,
                    "request_expired",
                    "request deadline elapsed before verified satisfaction",
                );
            }
        }
    }

    pub fn mark_unknown(
        &self,
        state: &mut State,
        run_id: Uuid,
        generation: u64,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let mut run = state
            .runs
            .get(&run_id)
            .cloned()
            .ok_or(DomainError::RunNotFound)?;
        ensure_active_owner(state, &run, generation)?;
        mark_unknown_inner(state, &mut run, reason, now)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Transitions carry explicit state, ownership fence, proof, and clock inputs"
    )]
    pub fn resolve_unknown(
        &self,
        state: &mut State,
        run_id: Uuid,
        generation: u64,
        resolution: UnknownResolution,
        command_id: Uuid,
        now: DateTime<Utc>,
        coverage: &CoverageContext,
    ) -> Result<(), DomainError> {
        let run = state
            .runs
            .get(&run_id)
            .cloned()
            .ok_or(DomainError::RunNotFound)?;
        match &resolution {
            UnknownResolution::VerifiedReceipt {
                receipt,
                evidence_digest,
            } => {
                validate_evidence(evidence_digest)?;
                if run.generation == generation
                    && receipt.run_id == run_id
                    && receipt.generation == generation
                    && state.receipts.get(&receipt.id) == Some(receipt)
                {
                    return Ok(());
                }
            }
            UnknownResolution::KnownStopped {
                executor_ref,
                evidence_digest,
            } if matches!(run.state, RunState::Canceled | RunState::Superseded) => {
                validate_evidence(evidence_digest)?;
                let supplied = executor_ref.as_deref().or(run.executor_ref.as_deref());
                if run.generation == generation
                    && supplied == run.executor_ref.as_deref()
                    && state.events.iter().any(|event| {
                        event.run_id == Some(run_id)
                            && matches!(event.kind.as_str(), "run_canceled" | "run_superseded")
                    })
                {
                    return Ok(());
                }
            }
            _ => {}
        }
        ensure_active_owner(state, &run, generation)?;
        if run.state != RunState::Unknown {
            if let UnknownResolution::KnownRunning {
                executor_ref,
                evidence_digest,
            } = &resolution
            {
                validate_evidence(evidence_digest)?;
                if matches!(run.state, RunState::Running | RunState::CancelRequested)
                    && run.executor_ref.as_deref() == Some(executor_ref)
                {
                    return Ok(());
                }
            }
            return Err(DomainError::InvalidTransition);
        }
        match resolution {
            UnknownResolution::KnownRunning {
                executor_ref,
                evidence_digest,
            } => {
                validate_evidence(&evidence_digest)?;
                if run
                    .executor_ref
                    .as_deref()
                    .is_some_and(|current| current != executor_ref)
                {
                    return Err(DomainError::ExecutorMismatch);
                }
                let stored = state.runs.get_mut(&run_id).expect("run already validated");
                stored.executor_ref = Some(executor_ref);
                stored.state = if run.cancel_reason.is_some() {
                    RunState::CancelRequested
                } else {
                    RunState::Running
                };
                stored.updated_at = now;
                clear_unknown_block(state, &run.lane_id);
                emit(
                    state,
                    &run.lane_id,
                    Some(run_id),
                    None,
                    now,
                    "unknown_resolved_running",
                    &format!(
                        "external observation {} confirmed run is still active",
                        evidence_digest
                    ),
                );
            }
            UnknownResolution::KnownStopped {
                executor_ref,
                evidence_digest,
            } => {
                validate_evidence(&evidence_digest)?;
                let executor_ref = executor_ref.or(run.executor_ref.clone());
                self.confirm_stopped_proven(
                    state,
                    run_id,
                    generation,
                    executor_ref.as_deref(),
                    command_id,
                    now,
                    coverage,
                )?;
                emit(
                    state,
                    &run.lane_id,
                    Some(run_id),
                    None,
                    now,
                    "unknown_resolved_stopped",
                    &format!(
                        "external stop evidence {} released lane ownership",
                        evidence_digest
                    ),
                );
            }
            UnknownResolution::VerifiedReceipt {
                receipt,
                evidence_digest,
            } => {
                validate_evidence(&evidence_digest)?;
                self.executor_result(
                    state,
                    run_id,
                    generation,
                    run.executor_ref.as_deref().unwrap_or("verified-receipt"),
                    ExecutorResult::VerifiedReceipt(receipt),
                    now,
                    coverage,
                )?;
                emit(
                    state,
                    &run.lane_id,
                    Some(run_id),
                    None,
                    now,
                    "unknown_resolved_receipt",
                    &format!(
                        "verified environment receipt {} resolved unknown state",
                        evidence_digest
                    ),
                );
            }
        }
        Ok(())
    }

    pub fn set_draining(&self, state: &mut State, draining: bool, now: DateTime<Utc>) {
        if state.draining == draining {
            return;
        }
        state.draining = draining;
        let lane_ids = state.lanes.keys().cloned().collect::<Vec<_>>();
        for lane_id in lane_ids {
            emit(
                state,
                &lane_id,
                None,
                None,
                now,
                if draining {
                    "drain_started"
                } else {
                    "drain_canceled"
                },
                if draining {
                    "service drain requested; new runs will not start"
                } else {
                    "service drain cleared"
                },
            );
        }
    }

    fn accept_receipt(
        &self,
        state: &mut State,
        run: &DeliveryRun,
        receipt: DeploymentReceipt,
        now: DateTime<Utc>,
        coverage: &CoverageContext,
    ) -> Result<(), DomainError> {
        if receipt.run_id != run.id
            || receipt.generation != run.generation
            || receipt.source_revision != run.target.revision
            || receipt.profile_digest != run.target.profile_digest
            || receipt
                .component_digests
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                != run.target.components
            || receipt
                .component_digests
                .values()
                .any(|digest| digest.trim().is_empty())
            || receipt.verification_report.trim().is_empty()
            || receipt.observed_at < run.created_at
            || receipt.observed_at > now
        {
            return Err(DomainError::InvalidReceipt);
        }
        if let Some(previous) = state.receipts.get(&receipt.id) {
            return if previous == &receipt {
                Ok(())
            } else {
                Err(DomainError::InvalidReceipt)
            };
        }
        self.expire(state, now);
        let mut lane = state
            .lanes
            .get(&run.lane_id)
            .cloned()
            .ok_or_else(|| DomainError::LaneNotFound(run.lane_id.clone()))?;
        if lane.active_run_id != Some(run.id) || lane.generation != run.generation {
            return Err(DomainError::StaleGeneration);
        }
        let mut stored_run = run.clone();
        stored_run.updated_at = now;
        stored_run.receipt_id = Some(receipt.id);
        match receipt.result {
            ReceiptResult::Success => {
                stored_run.state = RunState::Succeeded;
                let actual = Target {
                    repository_id: run.target.repository_id.clone(),
                    revision: receipt.source_revision.clone(),
                    components: receipt.component_digests.keys().cloned().collect(),
                    profile_digest: receipt.profile_digest.clone(),
                    release_line_id: run.target.release_line_id.clone(),
                    config_version: run.target.config_version,
                    coverage_mode: run.target.coverage_mode.clone(),
                };
                let request_ids = state
                    .requests
                    .values()
                    .filter(|request| {
                        request.lane_id == run.lane_id
                            && !request.status.is_terminal()
                            && (request.current_run_id == Some(run.id)
                                || request.current_run_id.is_none())
                    })
                    .map(|request| request.id)
                    .collect::<Vec<_>>();
                for request_id in request_ids {
                    let Some(request) = state.requests.get(&request_id).cloned() else {
                        continue;
                    };
                    if request.deadline_at.is_some_and(|deadline| deadline <= now) {
                        if let Some(stored) = state.requests.get_mut(&request_id) {
                            stored.status = RequestStatus::Expired;
                            stored.current_run_id = None;
                            stored.waiting_reason = None;
                        }
                    } else if let Some(reason) = target_covers(
                        &actual,
                        &request.target,
                        &run.lane_id,
                        &request.lane_id,
                        coverage,
                    ) {
                        if let Some(stored) = state.requests.get_mut(&request_id) {
                            stored.status = RequestStatus::Satisfied;
                            stored.current_run_id = None;
                            stored.satisfied_by_run = Some(run.id);
                            stored.satisfied_by_receipt = Some(receipt.id);
                            stored.waiting_reason = None;
                        }
                        emit(
                            state,
                            &run.lane_id,
                            Some(run.id),
                            Some(request_id),
                            now,
                            "request_satisfied",
                            &format!("verified receipt {} covers request ({reason})", receipt.id),
                        );
                    } else if let Some(stored) = state.requests.get_mut(&request_id) {
                        stored.current_run_id = None;
                        stored.waiting_reason = Some("receipt_did_not_cover_request".to_owned());
                    }
                }
                lane.active_run_id = None;
                lane.blocked_reason = None;
                lane.failure_paused = false;
            }
            ReceiptResult::Failure => {
                stored_run.state = RunState::Failed;
                settle_failed_run(
                    state,
                    &stored_run,
                    now,
                    "verified environment receipt reported failure",
                );
                lane.active_run_id = None;
                lane.failure_paused = true;
                lane.blocked_reason = Some(FAILURE_BLOCK.to_owned());
            }
        }
        state.receipts.insert(receipt.id, receipt.clone());
        state.runs.insert(run.id, stored_run.clone());
        if let Some(successor_id) = run.replacement_run_id
            && let Some(successor) = state.runs.get_mut(&successor_id)
            && successor.state == RunState::Planned
        {
            successor.state = RunState::Canceled;
            successor.updated_at = now;
        }
        lane.generation = lane.generation.saturating_add(1);
        if lane.blocked_reason.as_deref() == Some(CONFIG_CHANGED_BLOCK) && !lane.failure_paused {
            lane.blocked_reason = None;
        }
        state.lanes.insert(lane.id.clone(), lane);
        emit(
            state,
            &run.lane_id,
            Some(run.id),
            None,
            now,
            if receipt.result == ReceiptResult::Success {
                "receipt_verified_success"
            } else {
                "receipt_verified_failure"
            },
            &format!(
                "receipt {} persisted with verified environment evidence",
                receipt.id
            ),
        );
        Ok(())
    }

    fn preemption_candidate(
        &self,
        state: &State,
        lane: &Lane,
        waiter_ids: &[Uuid],
        coverage: &CoverageContext,
    ) -> Option<ReleaseRequest> {
        let mut candidates = state
            .requests
            .values()
            .filter(|request| {
                request.lane_id == lane.id
                    && request.current_run_id.is_none()
                    && request.priority == Priority::Urgent
                    && matches!(
                        request.status,
                        RequestStatus::Accepted | RequestStatus::Waiting
                    )
            })
            .cloned()
            .collect::<Vec<_>>();
        candidates.sort_by_key(request_order_key);
        candidates.into_iter().find(|candidate| {
            waiter_ids.iter().all(|waiter_id| {
                state.requests.get(waiter_id).is_some_and(|waiter| {
                    target_covers(
                        &candidate.target,
                        &waiter.target,
                        &lane.id,
                        &waiter.lane_id,
                        coverage,
                    )
                    .is_some()
                })
            }) && target_covers(
                &candidate.target,
                &candidate.target,
                &lane.id,
                &candidate.lane_id,
                coverage,
            )
            .is_some()
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Transitions carry explicit state, ownership fence, proof, and clock inputs"
    )]
    fn request_preemption(
        &self,
        state: &mut State,
        lane: &Lane,
        mut active: DeliveryRun,
        candidate: &ReleaseRequest,
        new_run_id: Uuid,
        command_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<ReconcileOutcome, DomainError> {
        if state.runs.contains_key(&new_run_id) || state.commands.contains_key(&command_id) {
            return Err(DomainError::DuplicateId);
        }
        let successor = DeliveryRun {
            id: new_run_id,
            lane_id: lane.id.clone(),
            target: candidate.target.clone(),
            effective_priority: Priority::Urgent,
            state: RunState::Planned,
            phase: Phase::Preparing,
            generation: lane.generation + 1,
            executor_ref: None,
            replacement_run_id: None,
            cancel_reason: None,
            receipt_id: None,
            created_at: now,
            phase_started_at: now,
            updated_at: now,
        };
        active.state = RunState::CancelRequested;
        active.replacement_run_id = Some(successor.id);
        active.cancel_reason = Some("urgent_preemption".to_owned());
        active.updated_at = now;
        let command = new_command(
            command_id,
            active.id,
            active.generation,
            CommandKind::Cancel,
            now,
            &format!(
                "cancel:{}:{}:replacement:{}",
                active.id, active.generation, successor.id
            ),
        );
        state.runs.insert(active.id, active.clone());
        state.runs.insert(successor.id, successor.clone());
        state.commands.insert(command.id, command.clone());
        emit(
            state,
            &lane.id,
            Some(active.id),
            Some(candidate.id),
            now,
            "urgent_preemption_requested",
            &format!(
                "urgent candidate {} covers every active waiter; old run retains ownership pending stop proof",
                candidate.id
            ),
        );
        Ok(ReconcileOutcome::PreemptionRequested {
            old_run_id: active.id,
            new_run_id: successor.id,
            cancel_command_id: command.id,
        })
    }

    fn cancel_unneeded_run(
        &self,
        state: &mut State,
        lane: &Lane,
        mut run: DeliveryRun,
        command_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<ReconcileOutcome, DomainError> {
        if state.commands.contains_key(&command_id) {
            return Err(DomainError::DuplicateId);
        }
        run.state = RunState::CancelRequested;
        run.cancel_reason = Some("last_waiter_withdrew_or_expired".to_owned());
        run.updated_at = now;
        let command = new_command(
            command_id,
            run.id,
            run.generation,
            CommandKind::Cancel,
            now,
            &format!("cancel:{}:{}:no_waiters", run.id, run.generation),
        );
        state.runs.insert(run.id, run.clone());
        state.commands.insert(command.id, command.clone());
        emit(
            state,
            &lane.id,
            Some(run.id),
            None,
            now,
            "run_cancel_requested_no_waiters",
            "last waiter ended; cancellation requested while lane ownership remains fenced",
        );
        Ok(ReconcileOutcome::CancelRequested {
            run_id: run.id,
            command_id: command.id,
        })
    }
}

#[derive(Clone)]
struct CandidateBatch {
    candidate: ReleaseRequest,
    requests: Vec<ReleaseRequest>,
    anchor: ReleaseRequest,
    effective_priority: Priority,
}

fn choose_candidate_batch(
    candidates: &[ReleaseRequest],
    lane: &Lane,
    coverage: &CoverageContext,
) -> Option<CandidateBatch> {
    // Multiple requests can name the same immutable target. They have exactly
    // the same coverage set, so only the best-ordered request for that target
    // can affect the candidate tie-break. Keeping one representative avoids
    // rebuilding the same (potentially large) waiter vector once per duplicate.
    let mut candidates_by_target = BTreeMap::<_, &ReleaseRequest>::new();
    for candidate in candidates {
        candidates_by_target
            .entry(candidate_target_key(&candidate.target))
            .and_modify(|current| {
                if request_order_key(candidate) < request_order_key(current) {
                    *current = candidate;
                }
            })
            .or_insert(candidate);
    }

    let mut best_batch: Option<CandidateBatch> = None;
    for candidate in candidates_by_target.values() {
        let mut requests = candidates
            .iter()
            .filter(|request| {
                target_covers(
                    &candidate.target,
                    &request.target,
                    &lane.id,
                    &request.lane_id,
                    coverage,
                )
                .is_some()
            })
            .cloned()
            .collect::<Vec<_>>();
        if requests.is_empty() {
            continue;
        }
        requests.sort_by_key(request_order_key);
        let anchor = requests[0].clone();
        let effective_priority = requests
            .iter()
            .map(|request| request.priority)
            .max()
            .unwrap_or(candidate.priority);
        let batch = CandidateBatch {
            candidate: (**candidate).clone(),
            requests,
            anchor,
            effective_priority,
        };
        if best_batch
            .as_ref()
            .is_none_or(|best| candidate_batch_order_key(&batch) < candidate_batch_order_key(best))
        {
            best_batch = Some(batch);
        }
    }
    best_batch
}

type CandidateTargetKey<'a> = (
    &'a str,
    &'a str,
    &'a std::collections::BTreeSet<String>,
    &'a str,
    &'a str,
    u64,
    u8,
);

fn candidate_target_key(target: &Target) -> CandidateTargetKey<'_> {
    (
        &target.repository_id,
        &target.revision,
        &target.components,
        &target.profile_digest,
        &target.release_line_id,
        target.config_version,
        match &target.coverage_mode {
            crate::domain::CoverageMode::Exact => 0,
            crate::domain::CoverageMode::Descendant => 1,
        },
    )
}

type RequestOrderKey = (Reverse<Priority>, DateTime<Utc>, u64, Uuid);
type CandidateBatchOrderKey = (
    Reverse<Priority>,
    DateTime<Utc>,
    u64,
    Reverse<usize>,
    RequestOrderKey,
);

fn candidate_batch_order_key(batch: &CandidateBatch) -> CandidateBatchOrderKey {
    (
        Reverse(batch.effective_priority),
        batch.anchor.deadline_at.unwrap_or(DateTime::<Utc>::MAX_UTC),
        batch.anchor.creation_sequence,
        Reverse(batch.requests.len()),
        request_order_key(&batch.candidate),
    )
}

fn validate_target(
    lane: &Lane,
    target: &Target,
    authorization: &TargetAuthorization,
) -> Result<(), DomainError> {
    if !target_matches_lane(lane, target) {
        return Err(DomainError::TargetNotAllowed(
            "target repository, release line, profile, or config version differs from lane"
                .to_owned(),
        ));
    }
    if !is_full_revision(&target.revision) {
        return Err(DomainError::TargetNotAllowed(
            "revision must be a complete immutable Git object ID".to_owned(),
        ));
    }
    if target.components.is_empty() || !target.components.is_subset(&lane.allowed_components) {
        return Err(DomainError::TargetNotAllowed(
            "target components are empty or outside the lane allowlist".to_owned(),
        ));
    }
    if !lane.allowed_revisions.is_empty() && !lane.allowed_revisions.contains(&target.revision) {
        return Err(DomainError::TargetNotAllowed(
            "revision is outside the configured revision allowlist".to_owned(),
        ));
    }
    if target.coverage_mode == crate::domain::CoverageMode::Descendant
        && !lane.descendant_coverage_enabled
    {
        return Err(DomainError::TargetNotAllowed(
            "descendant coverage is disabled for this lane".to_owned(),
        ));
    }
    if authorization.repository_id != lane.repository_id
        || authorization.release_ref != lane.release_ref
        || authorization.revision != target.revision
        || authorization.release_line_id != lane.release_line_id
        || authorization.config_version != lane.config_version
        || authorization.relation != crate::domain::LineageRelation::Ancestor
        || !is_full_revision(&authorization.release_tip)
        || authorization.proof_digest.is_empty()
    {
        return Err(DomainError::LineageUnproven);
    }
    Ok(())
}

fn target_matches_lane(lane: &Lane, target: &Target) -> bool {
    target.repository_id == lane.repository_id
        && target.profile_digest == lane.profile_digest
        && target.release_line_id == lane.release_line_id
        && target.config_version == lane.config_version
        && !target.components.is_empty()
        && target.components.is_subset(&lane.allowed_components)
        && (lane.allowed_revisions.is_empty() || lane.allowed_revisions.contains(&target.revision))
        && (target.coverage_mode != crate::domain::CoverageMode::Descendant
            || lane.descendant_coverage_enabled)
}

fn target_covers(
    candidate: &Target,
    request: &Target,
    candidate_lane_id: &str,
    request_lane_id: &str,
    coverage: &CoverageContext,
) -> Option<String> {
    if candidate_lane_id != request_lane_id
        || candidate.repository_id != request.repository_id
        || candidate.release_line_id != request.release_line_id
        || candidate.config_version != request.config_version
        || candidate.profile_digest != request.profile_digest
        || !request.components.is_subset(&candidate.components)
        || !is_full_revision(&candidate.revision)
        || !is_full_revision(&request.revision)
    {
        return None;
    }
    if candidate.revision == request.revision {
        return Some("exact_revision_and_target_match".to_owned());
    }
    if request.coverage_mode != crate::domain::CoverageMode::Descendant {
        return None;
    }
    let proof = coverage.get(
        &candidate.revision,
        &request.revision,
        &candidate.repository_id,
        &candidate.release_line_id,
        candidate.config_version,
    )?;
    if proof.relation != crate::domain::LineageRelation::Ancestor || proof.proof_digest.is_empty() {
        return None;
    }
    Some(format!("trusted_git_descendant:{}", proof.proof_digest))
}

fn attach_to_covering_active_run(
    state: &mut State,
    lane: &Lane,
    request: &ReleaseRequest,
    now: DateTime<Utc>,
    coverage: &CoverageContext,
) -> Option<Uuid> {
    let run_id = lane.active_run_id?;
    let run = state.runs.get(&run_id)?.clone();
    if !run.state.owns_execution_slot()
        || run.state == RunState::Unknown
        || run.state == RunState::CancelRequested
        || request.deadline_at.is_some_and(|deadline| deadline <= now)
        || target_covers(
            &run.target,
            &request.target,
            &lane.id,
            &request.lane_id,
            coverage,
        )
        .is_none()
    {
        return None;
    }
    if let Some(stored) = state.requests.get_mut(&request.id) {
        stored.current_run_id = Some(run_id);
        stored.status = RequestStatus::Waiting;
        stored.waiting_reason = None;
    }
    let priority = run_effective_priority(state, run_id);
    if let Some(stored) = state.runs.get_mut(&run_id) {
        stored.effective_priority = priority;
    }
    emit(
        state,
        &lane.id,
        Some(run_id),
        Some(request.id),
        now,
        if request.priority == Priority::Urgent {
            "urgent_request_attached"
        } else {
            "request_attached"
        },
        "request attached to an active run that covers its immutable target",
    );
    Some(run_id)
}

fn attach_covered_waiters(
    state: &mut State,
    lane: &Lane,
    run: &mut DeliveryRun,
    now: DateTime<Utc>,
    coverage: &CoverageContext,
) {
    let candidates = state
        .requests
        .values()
        .filter(|request| {
            request.lane_id == lane.id
                && request.current_run_id.is_none()
                && matches!(
                    request.status,
                    RequestStatus::Accepted | RequestStatus::Waiting
                )
                && request.deadline_at.is_none_or(|deadline| deadline > now)
                && target_covers(
                    &run.target,
                    &request.target,
                    &lane.id,
                    &request.lane_id,
                    coverage,
                )
                .is_some()
        })
        .map(|request| request.id)
        .collect::<Vec<_>>();
    for request_id in candidates {
        if let Some(request) = state.requests.get_mut(&request_id) {
            request.current_run_id = Some(run.id);
            request.waiting_reason = None;
        }
        emit(
            state,
            &lane.id,
            Some(run.id),
            Some(request_id),
            now,
            "request_attached",
            "reconcile attached a covered waiting request to the active run",
        );
    }
    run.effective_priority = run_effective_priority(state, run.id);
    if let Some(stored) = state.runs.get_mut(&run.id) {
        stored.effective_priority = run.effective_priority;
    }
}

fn request_ids_for_run(state: &State, run_id: Uuid) -> Vec<Uuid> {
    state
        .requests
        .values()
        .filter(|request| {
            request.current_run_id == Some(run_id)
                && !request.status.is_terminal()
                && request.status != RequestStatus::NeedsAttention
        })
        .map(|request| request.id)
        .collect()
}

fn run_effective_priority(state: &State, run_id: Uuid) -> Priority {
    state
        .requests
        .values()
        .filter(|request| request.current_run_id == Some(run_id) && !request.status.is_terminal())
        .map(|request| request.priority)
        .max()
        .unwrap_or(Priority::Normal)
}

fn request_order_key(request: &ReleaseRequest) -> RequestOrderKey {
    (
        Reverse(request.priority),
        request.deadline_at.unwrap_or(DateTime::<Utc>::MAX_UTC),
        request.creation_sequence,
        request.id,
    )
}

fn new_command(
    id: Uuid,
    run_id: Uuid,
    generation: u64,
    kind: CommandKind,
    created_at: DateTime<Utc>,
    payload: &str,
) -> Command {
    Command {
        id,
        run_id,
        generation,
        kind,
        delivery_state: CommandDeliveryState::Pending,
        payload_digest: hex::encode(Sha256::digest(payload.as_bytes())),
        created_at,
    }
}

fn emit(
    state: &mut State,
    lane_id: &str,
    run_id: Option<Uuid>,
    request_id: Option<Uuid>,
    timestamp: DateTime<Utc>,
    kind: &str,
    message: &str,
) -> u64 {
    let next = state
        .next_event_sequence
        .entry(lane_id.to_owned())
        .or_insert(1);
    let sequence = *next;
    *next += 1;
    state.events.push(Event {
        actor: None,
        sequence,
        lane_id: lane_id.to_owned(),
        run_id,
        request_id,
        timestamp,
        kind: kind.to_owned(),
        message: message.to_owned(),
    });
    sequence
}

fn lane_config_changed(existing: &Lane, desired: &Lane) -> bool {
    existing.project != desired.project
        || existing.environment != desired.environment
        || existing.repository_id != desired.repository_id
        || existing.release_ref != desired.release_ref
        || existing.release_line_id != desired.release_line_id
        || existing.profile_digest != desired.profile_digest
        || existing.config_version != desired.config_version
        || existing.descendant_coverage_enabled != desired.descendant_coverage_enabled
        || existing.preemption_enabled != desired.preemption_enabled
        || existing.allowed_revisions != desired.allowed_revisions
        || existing.allowed_components != desired.allowed_components
        || existing.enabled != desired.enabled
}

fn ensure_active_owner(
    state: &State,
    run: &DeliveryRun,
    generation: u64,
) -> Result<(), DomainError> {
    let lane = state
        .lanes
        .get(&run.lane_id)
        .ok_or_else(|| DomainError::LaneNotFound(run.lane_id.clone()))?;
    if run.generation != generation
        || lane.active_run_id != Some(run.id)
        || lane.generation != generation
    {
        return Err(DomainError::StaleGeneration);
    }
    Ok(())
}

fn mark_unknown_inner(
    state: &mut State,
    run: &mut DeliveryRun,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<(), DomainError> {
    let lane = state
        .lanes
        .get_mut(&run.lane_id)
        .ok_or_else(|| DomainError::LaneNotFound(run.lane_id.clone()))?;
    if lane.active_run_id != Some(run.id) || lane.generation != run.generation {
        return Err(DomainError::StaleGeneration);
    }
    let next_reason = if reason.trim().is_empty() {
        UNKNOWN_BLOCK.to_owned()
    } else {
        format!("{UNKNOWN_BLOCK}: {}", reason.trim())
    };
    let event_message = format!(
        "execution state unknown; lane ownership retained: {}",
        reason.trim()
    );
    if run.state == RunState::Unknown
        && (lane.blocked_reason.as_deref() == Some(&next_reason)
            || state
                .events
                .iter()
                .rev()
                .find(|event| event.run_id == Some(run.id) && event.kind == "run_unknown")
                .is_some_and(|event| event.message == event_message))
    {
        return Ok(());
    }
    run.state = RunState::Unknown;
    run.updated_at = now;
    if lane.blocked_reason.is_none() || is_unknown_block(lane.blocked_reason.as_deref()) {
        lane.blocked_reason = Some(if reason.trim().is_empty() {
            UNKNOWN_BLOCK.to_owned()
        } else {
            format!("{UNKNOWN_BLOCK}: {}", reason.trim())
        });
    }
    state.runs.insert(run.id, run.clone());
    emit(
        state,
        &run.lane_id,
        Some(run.id),
        None,
        now,
        "run_unknown",
        &format!(
            "execution state unknown; lane ownership retained: {}",
            reason.trim()
        ),
    );
    Ok(())
}

fn settle_failed_run(state: &mut State, run: &DeliveryRun, now: DateTime<Utc>, message: &str) {
    if let Some(lane) = state.lanes.get_mut(&run.lane_id) {
        lane.active_run_id = None;
        lane.failure_paused = true;
        lane.blocked_reason = Some(FAILURE_BLOCK.to_owned());
    }
    let request_ids = state
        .requests
        .values()
        .filter(|request| request.current_run_id == Some(run.id) && !request.status.is_terminal())
        .map(|request| request.id)
        .collect::<Vec<_>>();
    for request_id in request_ids {
        if let Some(request) = state.requests.get_mut(&request_id) {
            request.current_run_id = None;
            request.status = RequestStatus::NeedsAttention;
            request.waiting_reason = Some("run_failed".to_owned());
        }
        emit(
            state,
            &run.lane_id,
            Some(run.id),
            Some(request_id),
            now,
            "request_needs_attention",
            &format!(
                "run failed; request requires explicit retry: {}",
                message.trim()
            ),
        );
    }
    if let Some(successor_id) = run.replacement_run_id
        && let Some(successor) = state.runs.get_mut(&successor_id)
        && successor.state == RunState::Planned
    {
        successor.state = RunState::Canceled;
        successor.updated_at = now;
    }
    emit(
        state,
        &run.lane_id,
        Some(run.id),
        None,
        now,
        "run_failed",
        &format!("confirmed failure paused lane: {}", message.trim()),
    );
}

fn clear_unknown_block(state: &mut State, lane_id: &str) {
    if let Some(lane) = state.lanes.get_mut(lane_id)
        && is_unknown_block(lane.blocked_reason.as_deref())
    {
        lane.blocked_reason = None;
    }
}

fn is_unknown_block(reason: Option<&str>) -> bool {
    reason.is_some_and(|value| {
        value == UNKNOWN_BLOCK || value.starts_with("execution ownership is unknown:")
    })
}

fn validate_evidence(evidence_digest: &str) -> Result<(), DomainError> {
    if evidence_digest.trim().is_empty() {
        return Err(DomainError::OwnershipUnproven);
    }
    Ok(())
}

fn set_waiting_reason(state: &mut State, lane_id: &str, reason: &str) {
    for request in state.requests.values_mut().filter(|request| {
        request.lane_id == lane_id
            && request.current_run_id.is_none()
            && !request.status.is_terminal()
    }) {
        request.waiting_reason = Some(reason.to_owned());
    }
}

fn mark_stale_lane_requests(state: &mut State, lane: &Lane, now: DateTime<Utc>) {
    let stale_ids = state
        .requests
        .values()
        .filter(|request| {
            request.lane_id == lane.id
                && request.current_run_id.is_none()
                && matches!(
                    request.status,
                    RequestStatus::Accepted | RequestStatus::Waiting
                )
                && !target_matches_lane(lane, &request.target)
        })
        .map(|request| request.id)
        .collect::<Vec<_>>();
    for request_id in stale_ids {
        let Some(request) = state.requests.get_mut(&request_id) else {
            continue;
        };
        request.status = RequestStatus::NeedsAttention;
        request.waiting_reason = Some("lane_configuration_changed".to_owned());
        emit(
            state,
            &lane.id,
            None,
            Some(request_id),
            now,
            "request_configuration_stale",
            "queued target no longer matches the configured lane snapshot",
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use chrono::{Duration, TimeZone};

    use super::{Engine, EngineConfig};
    use crate::{
        domain::{
            CommandDelivery, CoverageMode, DeploymentReceipt, ExecutorResult, Lane,
            LineageRelation, Phase, Priority, ReceiptResult, ReconcileIds, ReconcileOutcome,
            RequestStatus, RunState, State, SubmitInput, Target, TargetAuthorization,
            UnknownResolution,
        },
        lineage::{CoverageContext, LineageProof},
    };

    fn time(second: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .timestamp_opt(1_800_000_000 + second, 0)
            .unwrap()
    }

    fn ids(value: u128) -> uuid::Uuid {
        uuid::Uuid::from_u128(value)
    }

    fn lane(id: &str) -> Lane {
        Lane {
            tenant_id: crate::domain::default_local_tenant(),
            id: id.to_owned(),
            project: "project".to_owned(),
            environment: "test".to_owned(),
            enabled: true,
            repository_id: "repo".to_owned(),
            release_ref: "refs/heads/main".to_owned(),
            release_line_id: "stable".to_owned(),
            profile_digest: "profile-1".to_owned(),
            config_version: 1,
            descendant_coverage_enabled: true,
            preemption_enabled: true,
            allowed_revisions: BTreeSet::new(),
            allowed_components: BTreeSet::from(["app".to_owned(), "worker".to_owned()]),
            active_run_id: None,
            generation: 0,
            blocked_reason: None,
            failure_paused: false,
        }
    }

    fn target(revision: &str, components: &[&str], coverage_mode: CoverageMode) -> Target {
        Target {
            repository_id: "repo".to_owned(),
            revision: revision.to_owned(),
            components: components.iter().map(|value| (*value).to_owned()).collect(),
            profile_digest: "profile-1".to_owned(),
            release_line_id: "stable".to_owned(),
            config_version: 1,
            coverage_mode,
        }
    }

    fn authorization(revision: &str) -> TargetAuthorization {
        TargetAuthorization {
            repository_id: "repo".to_owned(),
            release_ref: "refs/heads/main".to_owned(),
            release_tip: revision.to_owned(),
            revision: revision.to_owned(),
            release_line_id: "stable".to_owned(),
            config_version: 1,
            relation: LineageRelation::Ancestor,
            proof_digest: "trusted-authorization-proof".to_owned(),
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Fixture inputs keep each ownership and target boundary explicit"
    )]
    fn submit(
        state: &mut State,
        engine: &Engine,
        request_id: u128,
        lane_id: &str,
        revision: &str,
        components: &[&str],
        mode: CoverageMode,
        priority: Priority,
        at: i64,
        coverage: &CoverageContext,
    ) -> uuid::Uuid {
        engine
            .submit(
                state,
                SubmitInput {
                    context: Default::default(),
                    id: ids(request_id),
                    principal: format!("caller-{request_id}"),
                    idempotency_key: format!("idem-{request_id}"),
                    request_hash: format!("body-{request_id}"),
                    lane_id: lane_id.to_owned(),
                    target: target(revision, components, mode),
                    authorization: authorization(revision),
                    priority,
                    deadline_at: None,
                    reason: "test request".to_owned(),
                },
                time(at),
                coverage,
            )
            .unwrap()
            .request_id
    }

    fn proof(candidate: &str, requested: &str, relation: LineageRelation) -> LineageProof {
        LineageProof {
            repository_id: "repo".to_owned(),
            candidate_revision: candidate.to_owned(),
            requested_revision: requested.to_owned(),
            release_line_id: "stable".to_owned(),
            config_version: 1,
            relation,
            proof_digest: "proof-digest".to_owned(),
        }
    }

    fn context(entries: &[(&str, &str, LineageRelation)]) -> CoverageContext {
        CoverageContext::from_proofs(
            entries.iter().map(|(candidate, requested, relation)| {
                proof(candidate, requested, relation.clone())
            }),
        )
    }

    fn setup(engine: &Engine, lane_id: &str) -> State {
        let mut state = State::default();
        engine
            .register_lane(&mut state, lane(lane_id), time(0))
            .unwrap();
        state
    }

    fn dispatched_run(outcome: ReconcileOutcome) -> uuid::Uuid {
        match outcome {
            ReconcileOutcome::Dispatched { run_id, .. } => run_id,
            other => panic!("expected dispatch, got {other:?}"),
        }
    }

    #[test]
    fn coverage_requires_same_lane_profile_components_mode_and_git_proof() {
        let engine = Engine::new(EngineConfig {
            coalesce_window: Duration::zero(),
            ..EngineConfig::default()
        });
        let mut state = setup(&engine, "lane-a");
        engine
            .register_lane(&mut state, lane("lane-b"), time(0))
            .unwrap();
        let no_proof = CoverageContext::default();
        submit(
            &mut state,
            &engine,
            1,
            "lane-a",
            &"a".repeat(40),
            &["app", "worker"],
            CoverageMode::Descendant,
            Priority::Normal,
            1,
            &no_proof,
        );
        let first_run = dispatched_run(
            engine
                .reconcile(
                    &mut state,
                    "lane-a",
                    time(2),
                    ReconcileIds {
                        run_id: ids(100),
                        command_id: ids(101),
                    },
                    &no_proof,
                )
                .unwrap(),
        );
        engine
            .executor_result(
                &mut state,
                first_run,
                1,
                "exec-a",
                ExecutorResult::Started {
                    executor_ref: "exec-a".to_owned(),
                },
                time(3),
                &no_proof,
            )
            .unwrap();

        let ancestor = "a".repeat(40);
        let descendant = "b".repeat(40);
        let descendant_proof = context(&[(&descendant, &ancestor, LineageRelation::Ancestor)]);
        let descendant_request = submit(
            &mut state,
            &engine,
            2,
            "lane-a",
            &ancestor,
            &["app"],
            CoverageMode::Descendant,
            Priority::Urgent,
            4,
            &descendant_proof,
        );
        assert_eq!(
            state.requests[&descendant_request].current_run_id,
            Some(first_run),
            "active run covers an explicitly descendant-enabled narrower request"
        );
        assert_eq!(state.runs[&first_run].effective_priority, Priority::Urgent);
        assert_eq!(state.runs[&first_run].phase_started_at, time(3));
        assert_eq!(state.runs[&first_run].updated_at, time(3));

        let third = "c".repeat(40);
        submit(
            &mut state,
            &engine,
            3,
            "lane-a",
            &third,
            &["app"],
            CoverageMode::Exact,
            Priority::Normal,
            5,
            &descendant_proof,
        );
        assert_eq!(
            state.requests[&ids(3)].current_run_id,
            None,
            "exact mode cannot use a descendant proof"
        );
        submit(
            &mut state,
            &engine,
            4,
            "lane-b",
            &ancestor,
            &["app"],
            CoverageMode::Descendant,
            Priority::Normal,
            6,
            &descendant_proof,
        );
        assert_eq!(
            state.requests[&ids(4)].current_run_id,
            None,
            "lane boundary is strict"
        );
        let idle = engine
            .reconcile(
                &mut state,
                "lane-a",
                time(7),
                ReconcileIds {
                    run_id: ids(500),
                    command_id: ids(501),
                },
                &descendant_proof,
            )
            .unwrap();
        assert_eq!(idle, ReconcileOutcome::Idle);
        assert_eq!(state.runs[&first_run].phase_started_at, time(3));
        assert_eq!(state.runs[&first_run].updated_at, time(3));
    }

    #[test]
    fn idempotency_conflict_and_priority_attachment_are_audited() {
        let engine = Engine::new(EngineConfig {
            coalesce_window: Duration::zero(),
            ..EngineConfig::default()
        });
        let mut state = setup(&engine, "lane-a");
        let revision = "a".repeat(40);
        let request_id = submit(
            &mut state,
            &engine,
            1,
            "lane-a",
            &revision,
            &["app"],
            CoverageMode::Exact,
            Priority::Normal,
            1,
            &CoverageContext::default(),
        );
        let replay = engine
            .submit(
                &mut state,
                SubmitInput {
                    context: Default::default(),
                    id: ids(99),
                    principal: "caller-1".to_owned(),
                    idempotency_key: "idem-1".to_owned(),
                    request_hash: "body-1".to_owned(),
                    lane_id: "lane-a".to_owned(),
                    target: target(&revision, &["app"], CoverageMode::Exact),
                    authorization: authorization(&revision),
                    priority: Priority::Normal,
                    deadline_at: None,
                    reason: "test request".to_owned(),
                },
                time(2),
                &CoverageContext::default(),
            )
            .unwrap();
        assert!(replay.reused);
        assert_eq!(replay.request_id, request_id);
        let conflict = engine.submit(
            &mut state,
            SubmitInput {
                context: Default::default(),
                id: ids(100),
                principal: "caller-1".to_owned(),
                idempotency_key: "idem-1".to_owned(),
                request_hash: "different".to_owned(),
                lane_id: "lane-a".to_owned(),
                target: target(&revision, &["app"], CoverageMode::Exact),
                authorization: authorization(&revision),
                priority: Priority::Normal,
                deadline_at: None,
                reason: "test request".to_owned(),
            },
            time(3),
            &CoverageContext::default(),
        );
        assert_eq!(
            conflict.unwrap_err(),
            crate::domain::DomainError::IdempotencyConflict
        );

        engine
            .set_priority(
                &mut state,
                request_id,
                Priority::Urgent,
                "incident".to_owned(),
                time(4),
            )
            .unwrap();
        assert_eq!(state.requests[&request_id].priority, Priority::Urgent);
        assert!(
            state
                .events
                .iter()
                .any(|event| event.kind == "request_priority_raised")
        );
    }

    #[test]
    fn queued_requests_from_an_old_lane_snapshot_are_not_dispatched() {
        let engine = Engine::new(EngineConfig {
            coalesce_window: Duration::zero(),
            ..EngineConfig::default()
        });
        let mut state = setup(&engine, "lane-a");
        let revision = "a".repeat(40);
        let request_id = submit(
            &mut state,
            &engine,
            1,
            "lane-a",
            &revision,
            &["app"],
            CoverageMode::Exact,
            Priority::Normal,
            1,
            &CoverageContext::default(),
        );
        let mut changed_lane = lane("lane-a");
        changed_lane.config_version = 2;
        changed_lane.profile_digest = "profile-2".to_owned();
        engine
            .register_lane(&mut state, changed_lane, time(2))
            .unwrap();
        let outcome = engine
            .reconcile(
                &mut state,
                "lane-a",
                time(3),
                ReconcileIds {
                    run_id: ids(100),
                    command_id: ids(101),
                },
                &CoverageContext::default(),
            )
            .unwrap();
        assert_eq!(outcome, ReconcileOutcome::Idle);
        assert_eq!(
            state.requests[&request_id].status,
            RequestStatus::NeedsAttention
        );
        assert_eq!(
            state.requests[&request_id].waiting_reason.as_deref(),
            Some("lane_configuration_changed")
        );
        assert!(state.runs.is_empty());
    }

    #[test]
    fn queued_requests_batch_only_from_existing_covering_target_and_deadline_order() {
        let engine = Engine::new(EngineConfig {
            coalesce_window: Duration::seconds(2),
            ..EngineConfig::default()
        });
        let mut state = setup(&engine, "lane-a");
        let old = "a".repeat(40);
        let new = "b".repeat(40);
        submit(
            &mut state,
            &engine,
            1,
            "lane-a",
            &old,
            &["app"],
            CoverageMode::Descendant,
            Priority::Normal,
            1,
            &CoverageContext::default(),
        );
        let deadline = time(30);
        let later_id = ids(2);
        engine
            .submit(
                &mut state,
                SubmitInput {
                    context: Default::default(),
                    id: later_id,
                    principal: "caller-2".to_owned(),
                    idempotency_key: "idem-2".to_owned(),
                    request_hash: "body-2".to_owned(),
                    lane_id: "lane-a".to_owned(),
                    target: target(&new, &["app", "worker"], CoverageMode::Exact),
                    authorization: authorization(&new),
                    priority: Priority::Normal,
                    deadline_at: Some(deadline),
                    reason: "deadline".to_owned(),
                },
                time(2),
                &CoverageContext::default(),
            )
            .unwrap();
        let proofs = context(&[(&new, &old, LineageRelation::Ancestor)]);
        let waiting = engine
            .reconcile(
                &mut state,
                "lane-a",
                time(2),
                ReconcileIds {
                    run_id: ids(101),
                    command_id: ids(102),
                },
                &proofs,
            )
            .unwrap();
        assert!(matches!(
            waiting,
            ReconcileOutcome::WaitingForMergeWindow { .. }
        ));
        let dispatched = engine
            .reconcile(
                &mut state,
                "lane-a",
                time(4),
                ReconcileIds {
                    run_id: ids(101),
                    command_id: ids(102),
                },
                &proofs,
            )
            .unwrap();
        assert!(matches!(dispatched, ReconcileOutcome::Dispatched { .. }));
        assert_eq!(state.runs[&ids(101)].target.revision, new);
        assert_eq!(state.requests[&ids(1)].current_run_id, Some(ids(101)));
        assert_eq!(state.requests[&later_id].current_run_id, Some(ids(101)));
    }

    #[test]
    fn duplicate_targets_keep_deterministic_candidate_and_waiter_order() {
        const DUPLICATE_COUNT: usize = 256;

        let engine = Engine::new(EngineConfig {
            coalesce_window: Duration::zero(),
            ..EngineConfig::default()
        });
        let mut state = setup(&engine, "lane-a");
        let old = "a".repeat(40);
        let covering = "b".repeat(40);
        let mut duplicate_ids = Vec::with_capacity(DUPLICATE_COUNT);

        for index in 0..DUPLICATE_COUNT {
            let priority = if index == 1 || index == 47 {
                Priority::Urgent
            } else {
                Priority::Normal
            };
            let request_id = submit(
                &mut state,
                &engine,
                10_000 + index as u128,
                "lane-a",
                &old,
                &["app"],
                CoverageMode::Descendant,
                priority,
                1,
                &CoverageContext::default(),
            );
            let deadline = match index {
                1 => 9,
                47 => 3,
                _ => 100 + index as i64,
            };
            state.requests.get_mut(&request_id).unwrap().deadline_at = Some(time(deadline));
            duplicate_ids.push(request_id);
        }

        let covering_id = submit(
            &mut state,
            &engine,
            20_000,
            "lane-a",
            &covering,
            &["app", "worker"],
            CoverageMode::Exact,
            Priority::Normal,
            1,
            &CoverageContext::default(),
        );
        let proofs = context(&[(&covering, &old, LineageRelation::Ancestor)]);
        let lane = state.lanes.get("lane-a").unwrap();

        let duplicate_only = state
            .requests
            .values()
            .filter(|request| request.id != covering_id)
            .cloned()
            .collect::<Vec<_>>();
        let duplicate_batch = super::choose_candidate_batch(&duplicate_only, lane, &proofs)
            .expect("duplicate target group has a candidate");
        assert_eq!(duplicate_batch.candidate.id, duplicate_ids[47]);
        assert_eq!(duplicate_batch.anchor.id, duplicate_ids[47]);
        assert_eq!(duplicate_batch.effective_priority, Priority::Urgent);
        assert_eq!(duplicate_batch.requests.len(), DUPLICATE_COUNT);

        let queries = engine.coverage_queries(&state, "lane-a");
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].candidate_revision, covering);
        assert_eq!(queries[0].requested_revision, old);

        let queued = state.requests.values().cloned().collect::<Vec<_>>();
        let batch = super::choose_candidate_batch(&queued, lane, &proofs)
            .expect("covering target has a candidate batch");
        assert_eq!(batch.candidate.id, covering_id);
        assert_eq!(batch.anchor.id, duplicate_ids[47]);
        assert_eq!(batch.effective_priority, Priority::Urgent);

        let mut expected = vec![duplicate_ids[47], duplicate_ids[1]];
        expected.extend(
            (0..DUPLICATE_COUNT)
                .filter(|index| *index != 1 && *index != 47)
                .map(|index| duplicate_ids[index]),
        );
        expected.push(covering_id);
        assert_eq!(
            batch
                .requests
                .iter()
                .map(|request| request.id)
                .collect::<Vec<_>>(),
            expected
        );

        let outcome = engine
            .reconcile(
                &mut state,
                "lane-a",
                time(2),
                ReconcileIds {
                    run_id: ids(30_000),
                    command_id: ids(30_001),
                },
                &proofs,
            )
            .unwrap();
        assert_eq!(
            outcome,
            ReconcileOutcome::Dispatched {
                run_id: ids(30_000),
                command_id: ids(30_001),
                attached_requests: expected,
            }
        );
        assert_eq!(state.runs[&ids(30_000)].target.revision, covering);
    }

    #[test]
    fn queued_exact_request_is_not_coalesced_into_a_descendant_candidate() {
        let engine = Engine::new(EngineConfig {
            coalesce_window: Duration::seconds(2),
            ..EngineConfig::default()
        });
        let mut state = setup(&engine, "lane-a");
        let exact = "a".repeat(40);
        let descendant = "b".repeat(40);
        let exact_request = submit(
            &mut state,
            &engine,
            1,
            "lane-a",
            &exact,
            &["app"],
            CoverageMode::Exact,
            Priority::Normal,
            1,
            &CoverageContext::default(),
        );
        let descendant_request = submit(
            &mut state,
            &engine,
            2,
            "lane-a",
            &descendant,
            &["app"],
            CoverageMode::Descendant,
            Priority::Normal,
            2,
            &CoverageContext::default(),
        );
        let proofs = context(&[(&descendant, &exact, LineageRelation::Ancestor)]);
        let dispatch = engine
            .reconcile(
                &mut state,
                "lane-a",
                time(3),
                ReconcileIds {
                    run_id: ids(101),
                    command_id: ids(102),
                },
                &proofs,
            )
            .unwrap();
        assert_eq!(
            dispatch,
            ReconcileOutcome::Dispatched {
                run_id: ids(101),
                command_id: ids(102),
                attached_requests: vec![exact_request],
            }
        );
        assert_eq!(state.runs[&ids(101)].target.revision, exact);
        assert_eq!(state.requests[&descendant_request].current_run_id, None);
    }

    #[test]
    fn three_queued_descendants_choose_the_submitted_target_that_covers_all() {
        let engine = Engine::new(EngineConfig {
            coalesce_window: Duration::seconds(2),
            ..EngineConfig::default()
        });
        let mut state = setup(&engine, "lane-a");
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let c = "c".repeat(40);
        let requests = [
            submit(
                &mut state,
                &engine,
                1,
                "lane-a",
                &a,
                &["app"],
                CoverageMode::Descendant,
                Priority::Normal,
                1,
                &CoverageContext::default(),
            ),
            submit(
                &mut state,
                &engine,
                2,
                "lane-a",
                &b,
                &["app"],
                CoverageMode::Descendant,
                Priority::Normal,
                2,
                &CoverageContext::default(),
            ),
            submit(
                &mut state,
                &engine,
                3,
                "lane-a",
                &c,
                &["app"],
                CoverageMode::Exact,
                Priority::Normal,
                2,
                &CoverageContext::default(),
            ),
        ];
        let proofs = context(&[
            (&b, &a, LineageRelation::Ancestor),
            (&c, &a, LineageRelation::Ancestor),
            (&c, &b, LineageRelation::Ancestor),
        ]);
        let dispatch = engine
            .reconcile(
                &mut state,
                "lane-a",
                time(3),
                ReconcileIds {
                    run_id: ids(101),
                    command_id: ids(102),
                },
                &proofs,
            )
            .unwrap();
        assert_eq!(
            dispatch,
            ReconcileOutcome::Dispatched {
                run_id: ids(101),
                command_id: ids(102),
                attached_requests: requests.to_vec(),
            }
        );
        assert_eq!(state.runs[&ids(101)].target.revision, c);
    }

    #[test]
    fn urgent_preemption_keeps_old_owner_until_stopped_then_transfers_generation() {
        let engine = Engine::new(EngineConfig {
            coalesce_window: Duration::zero(),
            ..EngineConfig::default()
        });
        let mut state = setup(&engine, "lane-a");
        let old_revision = "a".repeat(40);
        let urgent_revision = "b".repeat(40);
        let old_request = submit(
            &mut state,
            &engine,
            1,
            "lane-a",
            &old_revision,
            &["app"],
            CoverageMode::Descendant,
            Priority::Normal,
            1,
            &CoverageContext::default(),
        );
        let old_run = dispatched_run(
            engine
                .reconcile(
                    &mut state,
                    "lane-a",
                    time(2),
                    ReconcileIds {
                        run_id: ids(100),
                        command_id: ids(101),
                    },
                    &CoverageContext::default(),
                )
                .unwrap(),
        );
        engine
            .executor_result(
                &mut state,
                old_run,
                1,
                "exec-old",
                ExecutorResult::Started {
                    executor_ref: "exec-old".to_owned(),
                },
                time(3),
                &CoverageContext::default(),
            )
            .unwrap();
        let proof = context(&[(&urgent_revision, &old_revision, LineageRelation::Ancestor)]);
        let urgent_request = submit(
            &mut state,
            &engine,
            2,
            "lane-a",
            &urgent_revision,
            &["app"],
            CoverageMode::Exact,
            Priority::Urgent,
            4,
            &proof,
        );
        let result = engine
            .reconcile(
                &mut state,
                "lane-a",
                time(5),
                ReconcileIds {
                    run_id: ids(200),
                    command_id: ids(201),
                },
                &proof,
            )
            .unwrap();
        assert!(matches!(
            result,
            ReconcileOutcome::PreemptionRequested { .. }
        ));
        assert_eq!(state.lanes["lane-a"].active_run_id, Some(old_run));
        assert_eq!(state.requests[&old_request].current_run_id, Some(old_run));
        assert_eq!(state.requests[&urgent_request].current_run_id, None);
        assert_eq!(state.runs[&old_run].state, RunState::CancelRequested);
        assert_eq!(state.runs[&ids(200)].state, RunState::Planned);

        engine
            .command_delivery(&mut state, ids(201), CommandDelivery::Acknowledged, time(6))
            .unwrap();
        assert_eq!(state.lanes["lane-a"].active_run_id, Some(old_run));
        assert_eq!(state.runs[&ids(200)].state, RunState::Planned);

        engine
            .confirm_stopped(
                &mut state,
                old_run,
                1,
                "exec-old",
                ids(202),
                time(7),
                &proof,
            )
            .unwrap();
        let event_count = state.events.len();
        engine
            .confirm_stopped(
                &mut state,
                old_run,
                1,
                "exec-old",
                ids(203),
                time(8),
                &proof,
            )
            .unwrap();
        assert_eq!(state.events.len(), event_count);
        assert_eq!(state.runs[&old_run].state, RunState::Superseded);
        assert_eq!(state.lanes["lane-a"].active_run_id, Some(ids(200)));
        assert_eq!(state.lanes["lane-a"].generation, 2);
        assert_eq!(state.runs[&ids(200)].state, RunState::Dispatching);
        assert_eq!(state.requests[&old_request].current_run_id, Some(ids(200)));
        assert_eq!(
            state.requests[&urgent_request].current_run_id,
            Some(ids(200))
        );
        assert_eq!(
            state.commands[&ids(202)].kind,
            crate::domain::CommandKind::Dispatch
        );
        assert!(state.events.iter().any(|event| {
            event.kind == "request_bound_to_run"
                && event.run_id == Some(old_run)
                && event.request_id == Some(old_request)
        }));
        for request_id in [old_request, urgent_request] {
            let transfer = state
                .events
                .iter()
                .find(|event| {
                    event.kind == "request_transferred_to_run"
                        && event.run_id == Some(ids(200))
                        && event.request_id == Some(request_id)
                })
                .expect("per-request transfer audit event is persisted");
            assert!(transfer.message.contains(&old_run.to_string()));
            assert!(transfer.message.contains(&ids(200).to_string()));
        }
    }

    #[test]
    fn preemption_handoff_does_not_dispatch_after_drain_disable_or_deadline() {
        let engine = Engine::new(EngineConfig {
            coalesce_window: Duration::zero(),
            ..EngineConfig::default()
        });
        for case in 0..3_u128 {
            let mut state = setup(&engine, "lane-a");
            let old_revision = "a".repeat(40);
            let urgent_revision = "b".repeat(40);
            let old_request = submit(
                &mut state,
                &engine,
                1 + case * 10,
                "lane-a",
                &old_revision,
                &["app"],
                CoverageMode::Descendant,
                Priority::Normal,
                1,
                &CoverageContext::default(),
            );
            let old_run = dispatched_run(
                engine
                    .reconcile(
                        &mut state,
                        "lane-a",
                        time(2),
                        ReconcileIds {
                            run_id: ids(100 + case * 10),
                            command_id: ids(101 + case * 10),
                        },
                        &CoverageContext::default(),
                    )
                    .unwrap(),
            );
            engine
                .executor_result(
                    &mut state,
                    old_run,
                    1,
                    "exec-old",
                    ExecutorResult::Started {
                        executor_ref: "exec-old".to_owned(),
                    },
                    time(3),
                    &CoverageContext::default(),
                )
                .unwrap();
            let proof = context(&[(&urgent_revision, &old_revision, LineageRelation::Ancestor)]);
            let urgent_request = ids(2 + case * 10);
            engine
                .submit(
                    &mut state,
                    SubmitInput {
                        context: Default::default(),
                        id: urgent_request,
                        principal: format!("caller-{urgent_request}"),
                        idempotency_key: format!("idem-{urgent_request}"),
                        request_hash: format!("body-{urgent_request}"),
                        lane_id: "lane-a".to_owned(),
                        target: target(&urgent_revision, &["app"], CoverageMode::Exact),
                        authorization: authorization(&urgent_revision),
                        priority: Priority::Urgent,
                        deadline_at: (case == 2).then_some(time(6)),
                        reason: "urgent preemption race test".to_owned(),
                    },
                    time(4),
                    &proof,
                )
                .unwrap();
            assert!(matches!(
                engine
                    .reconcile(
                        &mut state,
                        "lane-a",
                        time(5),
                        ReconcileIds {
                            run_id: ids(200 + case * 10),
                            command_id: ids(201 + case * 10),
                        },
                        &proof,
                    )
                    .unwrap(),
                ReconcileOutcome::PreemptionRequested { .. }
            ));

            match case {
                0 => engine.set_draining(&mut state, true, time(6)),
                1 => {
                    let mut disabled = lane("lane-a");
                    disabled.enabled = false;
                    engine.register_lane(&mut state, disabled, time(6)).unwrap();
                }
                _ => {}
            }
            engine
                .confirm_stopped(
                    &mut state,
                    old_run,
                    1,
                    "exec-old",
                    ids(202 + case * 10),
                    time(7),
                    &proof,
                )
                .unwrap();

            assert_eq!(state.runs[&old_run].state, RunState::Canceled);
            assert_eq!(state.runs[&ids(200 + case * 10)].state, RunState::Canceled);
            assert_eq!(state.lanes["lane-a"].active_run_id, None);
            assert_eq!(state.requests[&old_request].current_run_id, None);
            assert_eq!(state.requests[&urgent_request].current_run_id, None);
            assert_eq!(
                state.requests[&urgent_request].status,
                if case == 2 {
                    RequestStatus::Expired
                } else {
                    RequestStatus::Waiting
                }
            );
        }
    }

    #[test]
    fn urgent_candidate_must_cover_all_active_waiters_and_inflight_phase_gate_wins() {
        let engine = Engine::new(EngineConfig {
            coalesce_window: Duration::zero(),
            ..EngineConfig::default()
        });
        let mut state = setup(&engine, "lane-a");
        let rev_a = "a".repeat(40);
        let rev_b = "b".repeat(40);
        let first = submit(
            &mut state,
            &engine,
            1,
            "lane-a",
            &rev_a,
            &["app"],
            CoverageMode::Descendant,
            Priority::Normal,
            1,
            &CoverageContext::default(),
        );
        let run_id = dispatched_run(
            engine
                .reconcile(
                    &mut state,
                    "lane-a",
                    time(2),
                    ReconcileIds {
                        run_id: ids(100),
                        command_id: ids(101),
                    },
                    &CoverageContext::default(),
                )
                .unwrap(),
        );
        engine
            .executor_result(
                &mut state,
                run_id,
                1,
                "exec",
                ExecutorResult::Started {
                    executor_ref: "exec".to_owned(),
                },
                time(3),
                &CoverageContext::default(),
            )
            .unwrap();
        let second = submit(
            &mut state,
            &engine,
            2,
            "lane-a",
            &rev_b,
            &["app"],
            CoverageMode::Exact,
            Priority::Normal,
            4,
            &CoverageContext::default(),
        );
        assert_eq!(state.requests[&second].current_run_id, None);
        let urgent = submit(
            &mut state,
            &engine,
            3,
            "lane-a",
            &"c".repeat(40),
            &["app"],
            CoverageMode::Exact,
            Priority::Urgent,
            5,
            &CoverageContext::default(),
        );
        let no_proofs = engine
            .reconcile(
                &mut state,
                "lane-a",
                time(6),
                ReconcileIds {
                    run_id: ids(200),
                    command_id: ids(201),
                },
                &CoverageContext::default(),
            )
            .unwrap();
        assert_eq!(no_proofs, ReconcileOutcome::Idle);
        assert_eq!(state.lanes["lane-a"].active_run_id, Some(run_id));
        assert_eq!(state.requests[&first].current_run_id, Some(run_id));
        assert_eq!(state.requests[&urgent].current_run_id, None);

        engine
            .phase_grant(&mut state, run_id, 1, "exec", Phase::Building, time(7))
            .unwrap();
        engine
            .phase_grant(&mut state, run_id, 1, "exec", Phase::Deploying, time(8))
            .unwrap();
        let proof = context(&[("c", "a", LineageRelation::Ancestor)]);
        let no_preempt = engine
            .reconcile(
                &mut state,
                "lane-a",
                time(9),
                ReconcileIds {
                    run_id: ids(300),
                    command_id: ids(301),
                },
                &proof,
            )
            .unwrap();
        assert_eq!(no_preempt, ReconcileOutcome::Idle);
        assert_eq!(state.runs[&run_id].phase, Phase::Deploying);
        assert_eq!(state.runs[&run_id].state, RunState::Running);
    }

    #[test]
    fn external_dispatch_is_generation_fenced_and_last_waiter_cannot_cancel_it() {
        let engine = Engine::new(EngineConfig {
            coalesce_window: Duration::zero(),
            ..EngineConfig::default()
        });
        let mut state = setup(&engine, "lane-a");
        let revision = "a".repeat(40);
        let request_id = submit(
            &mut state,
            &engine,
            1,
            "lane-a",
            &revision,
            &["app"],
            CoverageMode::Exact,
            Priority::Normal,
            1,
            &CoverageContext::default(),
        );
        let run_id = dispatched_run(
            engine
                .reconcile(
                    &mut state,
                    "lane-a",
                    time(2),
                    ReconcileIds {
                        run_id: ids(100),
                        command_id: ids(101),
                    },
                    &CoverageContext::default(),
                )
                .unwrap(),
        );

        let event_count = state.events.len();
        assert_eq!(
            engine.begin_external_dispatch(&mut state, run_id, 2, time(3)),
            Err(crate::domain::DomainError::StaleGeneration)
        );
        assert_eq!(state.events.len(), event_count);
        assert_eq!(state.runs[&run_id].state, RunState::Dispatching);
        assert_eq!(state.runs[&run_id].phase, Phase::Preparing);

        engine
            .begin_external_dispatch(&mut state, run_id, 1, time(3))
            .unwrap();
        let admitted_event_count = state.events.len();
        assert_eq!(state.runs[&run_id].state, RunState::Dispatching);
        assert_eq!(state.runs[&run_id].phase, Phase::Deploying);
        assert_eq!(state.runs[&run_id].phase_started_at, time(3));
        assert_eq!(state.runs[&run_id].updated_at, time(3));
        assert_eq!(state.runs[&run_id].executor_ref, None);
        assert_eq!(
            state.commands[&ids(101)].delivery_state,
            crate::domain::CommandDeliveryState::Pending
        );
        assert_eq!(
            state.events.last().map(|event| event.kind.as_str()),
            Some("external_dispatch_admitted")
        );
        assert_eq!(state.events.last().unwrap().run_id, Some(run_id));

        // Retrying the admission before the external call is idempotent and
        // cannot reset the duration clock or append another event.
        engine
            .begin_external_dispatch(&mut state, run_id, 1, time(4))
            .unwrap();
        assert_eq!(state.runs[&run_id].phase_started_at, time(3));
        assert_eq!(state.events.len(), admitted_event_count);

        engine
            .withdraw(&mut state, request_id, "caller-1", time(5))
            .unwrap();
        assert_eq!(
            engine
                .reconcile(
                    &mut state,
                    "lane-a",
                    time(6),
                    ReconcileIds {
                        run_id: ids(200),
                        command_id: ids(201),
                    },
                    &CoverageContext::default(),
                )
                .unwrap(),
            ReconcileOutcome::Idle
        );
        assert_eq!(state.lanes["lane-a"].active_run_id, Some(run_id));
        assert_eq!(state.runs[&run_id].state, RunState::Dispatching);
        assert_eq!(state.runs[&run_id].phase, Phase::Deploying);
        assert!(!state.commands.values().any(|command| {
            command.run_id == run_id && command.kind == crate::domain::CommandKind::Cancel
        }));

        // A later start acknowledgement completes dispatch without resetting
        // the clock for the already admitted non-preemptible phase.
        engine
            .executor_result(
                &mut state,
                run_id,
                1,
                "external-hook",
                ExecutorResult::Started {
                    executor_ref: "external-hook".to_owned(),
                },
                time(7),
                &CoverageContext::default(),
            )
            .unwrap();
        assert_eq!(state.runs[&run_id].state, RunState::Running);
        assert_eq!(state.runs[&run_id].phase, Phase::Deploying);
        assert_eq!(state.runs[&run_id].phase_started_at, time(3));
        assert_eq!(
            engine
                .reconcile(
                    &mut state,
                    "lane-a",
                    time(8),
                    ReconcileIds {
                        run_id: ids(202),
                        command_id: ids(203),
                    },
                    &CoverageContext::default(),
                )
                .unwrap(),
            ReconcileOutcome::Idle
        );
        assert_eq!(state.lanes["lane-a"].active_run_id, Some(run_id));
        assert_eq!(state.runs[&run_id].state, RunState::Running);
        assert_eq!(state.runs[&run_id].phase, Phase::Deploying);
        assert!(!state.commands.values().any(|command| {
            command.run_id == run_id && command.kind == crate::domain::CommandKind::Cancel
        }));
    }

    #[test]
    fn unknown_and_acknowledgement_keep_lane_owned_until_trusted_stop() {
        let engine = Engine::new(EngineConfig {
            coalesce_window: Duration::zero(),
            ..EngineConfig::default()
        });
        let mut state = setup(&engine, "lane-a");
        let revision = "a".repeat(40);
        submit(
            &mut state,
            &engine,
            1,
            "lane-a",
            &revision,
            &["app"],
            CoverageMode::Exact,
            Priority::Normal,
            1,
            &CoverageContext::default(),
        );
        let run_id = dispatched_run(
            engine
                .reconcile(
                    &mut state,
                    "lane-a",
                    time(2),
                    ReconcileIds {
                        run_id: ids(100),
                        command_id: ids(101),
                    },
                    &CoverageContext::default(),
                )
                .unwrap(),
        );
        engine
            .executor_result(
                &mut state,
                run_id,
                1,
                "exec",
                ExecutorResult::Started {
                    executor_ref: "exec".to_owned(),
                },
                time(3),
                &CoverageContext::default(),
            )
            .unwrap();
        engine
            .mark_unknown(&mut state, run_id, 1, "cancel response lost", time(4))
            .unwrap();
        assert_eq!(state.lanes["lane-a"].active_run_id, Some(run_id));
        assert_eq!(state.runs[&run_id].state, RunState::Unknown);
        let blocked = engine
            .reconcile(
                &mut state,
                "lane-a",
                time(5),
                ReconcileIds {
                    run_id: ids(200),
                    command_id: ids(201),
                },
                &CoverageContext::default(),
            )
            .unwrap();
        assert!(matches!(blocked, ReconcileOutcome::Blocked { .. }));
        engine
            .resolve_unknown(
                &mut state,
                run_id,
                1,
                UnknownResolution::KnownRunning {
                    executor_ref: "exec".to_owned(),
                    evidence_digest: "observe-1".to_owned(),
                },
                ids(202),
                time(6),
                &CoverageContext::default(),
            )
            .unwrap();
        assert_eq!(state.lanes["lane-a"].active_run_id, Some(run_id));
        assert_eq!(state.runs[&run_id].state, RunState::Running);
        engine
            .mark_unknown(&mut state, run_id, 1, "next observation lost", time(7))
            .unwrap();
        engine
            .resolve_unknown(
                &mut state,
                run_id,
                1,
                UnknownResolution::KnownStopped {
                    executor_ref: Some("exec".to_owned()),
                    evidence_digest: "stopped-observation".to_owned(),
                },
                ids(203),
                time(8),
                &CoverageContext::default(),
            )
            .unwrap();
        assert_eq!(state.runs[&run_id].state, RunState::Canceled);
        assert_eq!(state.lanes["lane-a"].active_run_id, None);
        assert_eq!(state.lanes["lane-a"].blocked_reason, None);
        let dispatched = engine
            .reconcile(
                &mut state,
                "lane-a",
                time(9),
                ReconcileIds {
                    run_id: ids(300),
                    command_id: ids(301),
                },
                &CoverageContext::default(),
            )
            .unwrap();
        assert!(matches!(dispatched, ReconcileOutcome::Dispatched { .. }));
    }

    #[test]
    fn verified_receipt_satisfies_each_covered_request_only_and_deadlines_are_terminal() {
        let engine = Engine::new(EngineConfig {
            coalesce_window: Duration::zero(),
            ..EngineConfig::default()
        });
        let mut state = setup(&engine, "lane-a");
        let revision = "a".repeat(40);
        let first = submit(
            &mut state,
            &engine,
            1,
            "lane-a",
            &revision,
            &["app", "worker"],
            CoverageMode::Exact,
            Priority::Normal,
            1,
            &CoverageContext::default(),
        );
        let second = submit(
            &mut state,
            &engine,
            2,
            "lane-a",
            &revision,
            &["app"],
            CoverageMode::Exact,
            Priority::Urgent,
            2,
            &CoverageContext::default(),
        );
        let run_id = dispatched_run(
            engine
                .reconcile(
                    &mut state,
                    "lane-a",
                    time(3),
                    ReconcileIds {
                        run_id: ids(100),
                        command_id: ids(101),
                    },
                    &CoverageContext::default(),
                )
                .unwrap(),
        );
        let receipt = DeploymentReceipt {
            id: ids(300),
            run_id,
            generation: 1,
            observed_at: time(4),
            source_revision: revision.clone(),
            component_digests: BTreeMap::from([
                ("app".to_owned(), "digest-app".to_owned()),
                ("worker".to_owned(), "digest-worker".to_owned()),
            ]),
            profile_digest: "profile-1".to_owned(),
            verification_report: "verified environment state".to_owned(),
            result: ReceiptResult::Success,
        };
        let duplicate_receipt = receipt.clone();
        engine
            .executor_result(
                &mut state,
                run_id,
                1,
                "scripted",
                ExecutorResult::Started {
                    executor_ref: "scripted".to_owned(),
                },
                time(3),
                &CoverageContext::default(),
            )
            .unwrap();
        engine
            .executor_result(
                &mut state,
                run_id,
                1,
                "scripted",
                ExecutorResult::VerifiedReceipt(receipt),
                time(5),
                &CoverageContext::default(),
            )
            .unwrap();
        engine
            .executor_result(
                &mut state,
                run_id,
                1,
                "scripted",
                ExecutorResult::VerifiedReceipt(duplicate_receipt.clone()),
                time(6),
                &CoverageContext::default(),
            )
            .unwrap();
        let mut conflicting_receipt = duplicate_receipt;
        conflicting_receipt.verification_report = "different payload".to_owned();
        assert_eq!(
            engine
                .executor_result(
                    &mut state,
                    run_id,
                    1,
                    "scripted",
                    ExecutorResult::VerifiedReceipt(conflicting_receipt),
                    time(7),
                    &CoverageContext::default(),
                )
                .unwrap_err(),
            crate::domain::DomainError::InvalidReceipt
        );
        assert_eq!(state.requests[&first].status, RequestStatus::Satisfied);
        assert_eq!(state.requests[&second].status, RequestStatus::Satisfied);
        assert_eq!(state.requests[&first].satisfied_by_run, Some(run_id));
        assert_eq!(state.requests[&first].satisfied_by_receipt, Some(ids(300)));
        assert_eq!(state.runs[&run_id].state, RunState::Succeeded);
        assert_eq!(state.lanes["lane-a"].active_run_id, None);
    }

    #[test]
    fn duplicate_started_event_does_not_reset_the_phase_clock() {
        let engine = Engine::new(EngineConfig {
            coalesce_window: Duration::zero(),
            ..EngineConfig::default()
        });
        let mut state = setup(&engine, "lane-a");
        let revision = "a".repeat(40);
        submit(
            &mut state,
            &engine,
            1,
            "lane-a",
            &revision,
            &["app"],
            CoverageMode::Exact,
            Priority::Normal,
            1,
            &CoverageContext::default(),
        );
        let run_id = dispatched_run(
            engine
                .reconcile(
                    &mut state,
                    "lane-a",
                    time(2),
                    ReconcileIds {
                        run_id: ids(100),
                        command_id: ids(101),
                    },
                    &CoverageContext::default(),
                )
                .unwrap(),
        );
        let started = ExecutorResult::Started {
            executor_ref: "exec".to_owned(),
        };
        engine
            .executor_result(
                &mut state,
                run_id,
                1,
                "exec",
                started.clone(),
                time(3),
                &CoverageContext::default(),
            )
            .unwrap();
        let phase_started_at = state.runs[&run_id].phase_started_at;
        engine
            .executor_result(
                &mut state,
                run_id,
                1,
                "exec",
                started,
                time(10),
                &CoverageContext::default(),
            )
            .unwrap();
        assert_eq!(state.runs[&run_id].phase_started_at, phase_started_at);
        assert_eq!(state.runs[&run_id].updated_at, time(3));
    }

    #[test]
    fn verifying_command_is_durable_once_and_sanitized_logs_are_events() {
        let engine = Engine::new(EngineConfig {
            coalesce_window: Duration::zero(),
            ..EngineConfig::default()
        });
        let mut state = setup(&engine, "lane-a");
        let revision = "a".repeat(40);
        submit(
            &mut state,
            &engine,
            1,
            "lane-a",
            &revision,
            &["app"],
            CoverageMode::Exact,
            Priority::Normal,
            1,
            &CoverageContext::default(),
        );
        let run_id = dispatched_run(
            engine
                .reconcile(
                    &mut state,
                    "lane-a",
                    time(2),
                    ReconcileIds {
                        run_id: ids(100),
                        command_id: ids(101),
                    },
                    &CoverageContext::default(),
                )
                .unwrap(),
        );
        engine
            .executor_result(
                &mut state,
                run_id,
                1,
                "exec",
                ExecutorResult::Started {
                    executor_ref: "exec".to_owned(),
                },
                time(3),
                &CoverageContext::default(),
            )
            .unwrap();
        for (phase, at) in [
            (Phase::Building, 4),
            (Phase::Deploying, 5),
            (Phase::Verifying, 6),
        ] {
            engine
                .phase_grant(&mut state, run_id, 1, "exec", phase, time(at))
                .unwrap();
        }
        let command = engine
            .request_verification(&mut state, run_id, 1, ids(200), time(7))
            .unwrap();
        let replay = engine
            .request_verification(&mut state, run_id, 1, ids(201), time(8))
            .unwrap();
        assert_eq!(command.id, ids(200));
        assert_eq!(replay, command);
        engine
            .command_delivery(
                &mut state,
                command.id,
                CommandDelivery::Acknowledged,
                time(9),
            )
            .unwrap();
        engine
            .record_executor_log(&mut state, run_id, "WARN", "verify\npassed", time(10))
            .unwrap();
        assert_eq!(state.lanes["lane-a"].active_run_id, Some(run_id));
        assert_eq!(
            state
                .events
                .iter()
                .filter(|event| event.kind == "verification_requested")
                .count(),
            1
        );
        assert_eq!(
            state
                .events
                .iter()
                .find(|event| event.kind == "executor_log")
                .unwrap()
                .message,
            "[warn] verify passed"
        );
    }

    #[test]
    fn failures_do_not_satisfy_or_retry_old_requests_automatically() {
        let engine = Engine::new(EngineConfig {
            coalesce_window: Duration::zero(),
            ..EngineConfig::default()
        });
        let mut state = setup(&engine, "lane-a");
        let revision = "a".repeat(40);
        let request_id = submit(
            &mut state,
            &engine,
            1,
            "lane-a",
            &revision,
            &["app"],
            CoverageMode::Exact,
            Priority::Normal,
            1,
            &CoverageContext::default(),
        );
        let run_id = dispatched_run(
            engine
                .reconcile(
                    &mut state,
                    "lane-a",
                    time(2),
                    ReconcileIds {
                        run_id: ids(100),
                        command_id: ids(101),
                    },
                    &CoverageContext::default(),
                )
                .unwrap(),
        );
        engine
            .executor_result(
                &mut state,
                run_id,
                1,
                "exec",
                ExecutorResult::Started {
                    executor_ref: "exec".to_owned(),
                },
                time(3),
                &CoverageContext::default(),
            )
            .unwrap();
        engine
            .executor_result(
                &mut state,
                run_id,
                1,
                "exec",
                ExecutorResult::Failed {
                    message: "build failed".to_owned(),
                    environment_known_unchanged: true,
                },
                time(4),
                &CoverageContext::default(),
            )
            .unwrap();
        assert_eq!(
            state.requests[&request_id].status,
            RequestStatus::NeedsAttention
        );
        assert_eq!(state.runs[&run_id].state, RunState::Failed);
        assert_eq!(
            state.lanes["lane-a"].blocked_reason.as_deref(),
            Some("failure_paused")
        );
        engine.retry(&mut state, request_id, time(5)).unwrap();
        assert_eq!(state.requests[&request_id].manual_retries_used, 1);
        assert_eq!(state.requests[&request_id].status, RequestStatus::Waiting);
        assert_eq!(state.requests[&request_id].target.revision, revision);
        assert!(!state.lanes["lane-a"].failure_paused);
    }
}
