use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    time::Duration,
};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{
    config::{Config, ConfigMode},
    domain::{
        CoverageMode, DeliveryRun, DeploymentReceipt, Priority, ReceiptResult, ReleaseRequest,
        RequestContext, RequestStatus, RunState, State,
    },
    lineage::is_full_revision,
    service::{RequestActor, RequestMutation, Service},
};

pub const LOCAL_PRINCIPAL: &str = "local-cli";
const MAX_REASON_BYTES: usize = 2 * 1024;
const MAX_COMPONENTS: usize = 64;
const MAX_COMPONENT_BYTES: usize = 128;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LocalError {
    #[error("select a lane with --lane when more than one lane is enabled")]
    LaneSelectionRequired,
    #[error("local configuration has no enabled lane")]
    NoEnabledLane,
    #[error("selected lane is not enabled")]
    LaneNotEnabled,
    #[error("cannot retry while execution state is unknown")]
    UnknownRetryNotAllowed,
}

pub struct Submission {
    pub lane_id: Option<String>,
    pub revision: String,
    pub components: Vec<String>,
    pub coverage_mode: CoverageMode,
    pub priority: Priority,
    pub deadline_at: Option<DateTime<Utc>>,
    pub reason: String,
    pub idempotency_key: Option<String>,
    pub context: RequestContext,
    pub parameters: BTreeMap<String, String>,
}

pub enum LocalAction {
    Submit(Box<Submission>),
    Resume { request_id: Uuid, retry: bool },
}

pub struct LocalSession {
    service: std::sync::Arc<Service>,
    request_id: Uuid,
    idempotency_key: String,
    reused: bool,
    tick_ms: u64,
}

pub struct LocalOutcome {
    pub exit_code: u8,
    pub report: Value,
}

impl LocalSession {
    pub async fn open(config: Config, action: LocalAction) -> anyhow::Result<Self> {
        if config.mode != ConfigMode::Local {
            bail!("run requires a local-mode configuration");
        }
        config.validate().context("validate local configuration")?;

        #[allow(
            clippy::large_enum_variant,
            reason = "Prepare one action before opening the shared local service"
        )]
        enum PreparedAction {
            Submit(Box<PreparedSubmission>),
            Resume { request_id: Uuid, retry: bool },
        }
        let action = match action {
            LocalAction::Submit(submission) => {
                let lane_id = select_lane(&config, submission.lane_id.as_deref())?;
                let lane_config = config
                    .lanes
                    .iter()
                    .find(|candidate| candidate.id == lane_id)
                    .expect("selected lane came from config");
                validate_submission(&submission, lane_config)?;
                PreparedAction::Submit(Box::new(prepare_submission(*submission, lane_id)?))
            }
            LocalAction::Resume { request_id, retry } => {
                PreparedAction::Resume { request_id, retry }
            }
        };
        let tick_ms = config.tick_ms;
        let project = config.lanes[0].project.clone();
        let service = Service::open(config).await?;

        let (request_id, idempotency_key, reused) = match action {
            PreparedAction::Submit(prepared) => {
                let result = service
                    .submit_with_parameters(
                        LOCAL_PRINCIPAL.to_owned(),
                        prepared.idempotency_key.clone(),
                        prepared.request_hash,
                        prepared.lane_id,
                        prepared.revision,
                        prepared.components,
                        prepared.coverage_mode,
                        prepared.priority,
                        prepared.deadline_at,
                        prepared.reason,
                        prepared.context,
                        prepared.parameters,
                    )
                    .await
                    .context("submit local release request")?;
                let snapshot = service.snapshot().await;
                let request = snapshot.requests.get(&result.request_id).ok_or_else(|| {
                    anyhow::anyhow!("submitted request is missing from the ledger")
                })?;
                if request.principal != LOCAL_PRINCIPAL
                    || !snapshot
                        .lanes
                        .get(&request.lane_id)
                        .is_some_and(|lane| lane.project.as_str() == project.as_str())
                {
                    bail!("submitted request is outside the local project scope");
                }
                (
                    result.request_id,
                    request.idempotency_key.clone(),
                    result.reused,
                )
            }
            PreparedAction::Resume { request_id, retry } => {
                let snapshot = service.snapshot().await;
                let request = snapshot
                    .requests
                    .get(&request_id)
                    .ok_or(crate::domain::DomainError::RequestNotFound)?;
                if request.principal != LOCAL_PRINCIPAL
                    || !snapshot
                        .lanes
                        .get(&request.lane_id)
                        .is_some_and(|lane| lane.project.as_str() == project.as_str())
                {
                    bail!("request is outside the local project scope");
                }
                let idempotency_key = request.idempotency_key.clone();
                if retry {
                    if request_has_unknown_run(&snapshot, request) {
                        return Err(LocalError::UnknownRetryNotAllowed.into());
                    }
                    let actor = RequestActor {
                        id: LOCAL_PRINCIPAL.to_owned(),
                        lanes: vec![request.lane_id.clone()],
                        projects: vec![project],
                        urgent: true,
                        admin: false,
                        executor: false,
                    };
                    service
                        .mutate_request(request_id, &actor, RequestMutation::Retry)
                        .await
                        .context("retry local release request")?;
                }
                (request_id, idempotency_key, true)
            }
        };

        Ok(Self {
            service,
            request_id,
            idempotency_key,
            reused,
            tick_ms,
        })
    }

    pub fn accepted(&self) -> Value {
        json!({
            "request_id": self.request_id,
            "idempotency_key": self.idempotency_key,
            "reused": self.reused,
        })
    }

    pub async fn wait(self, timeout: Duration) -> anyhow::Result<LocalOutcome> {
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| anyhow::anyhow!("local wait timeout is too large"))?;
        let initial = self.service.snapshot().await;
        if let Some(exit_code) = result_code(&initial, self.request_id, false) {
            self.service.begin_shutdown();
            return Ok(self.outcome(&initial, exit_code, false, false));
        }
        if timeout.is_zero() {
            self.service.begin_shutdown();
            let code = result_code(&initial, self.request_id, true).unwrap_or(124);
            return Ok(self.outcome(&initial, code, code == 124, false));
        }

        let mut interrupts = InterruptReceiver::new()?;
        type TickJoinResult =
            Result<Result<(), crate::service::ServiceError>, tokio::task::JoinError>;
        enum TickEvent {
            Deadline,
            Interrupted,
            Finished(TickJoinResult),
        }
        loop {
            if tokio::time::Instant::now() >= deadline {
                self.service.begin_shutdown();
                return Ok(self.outcome_from_current(124, true, false).await);
            }

            let service = self.service.clone();
            let mut tick: JoinHandle<Result<(), crate::service::ServiceError>> =
                tokio::spawn(async move { service.scheduler_tick().await });
            let tick_event = tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => TickEvent::Deadline,
                _ = interrupts.wait() => TickEvent::Interrupted,
                result = &mut tick => TickEvent::Finished(result),
            };
            match tick_event {
                TickEvent::Deadline => {
                    self.service.begin_shutdown();
                    // Keep the task alive until an already-started hook protocol
                    // handoff completes. A tick which has not started will see
                    // NotReady and perform no work.
                    let _ = tick.await;
                    return Ok(self.outcome_from_current(124, true, false).await);
                }
                TickEvent::Interrupted => {
                    self.service.begin_shutdown();
                    let _ = tick.await;
                    return Ok(self.outcome_from_current(130, false, true).await);
                }
                TickEvent::Finished(Ok(Ok(()))) => {}
                TickEvent::Finished(Ok(Err(error))) => {
                    return Err(anyhow::Error::new(error).context("run local scheduler tick"));
                }
                TickEvent::Finished(Err(error)) => {
                    return Err(anyhow::Error::new(error).context("join local scheduler tick"));
                }
            }

            let snapshot = self.service.snapshot().await;
            if let Some(exit_code) = result_code(&snapshot, self.request_id, true) {
                self.service.begin_shutdown();
                return Ok(self.outcome(&snapshot, exit_code, false, false));
            }

            let next_tick = tokio::time::Instant::now()
                .checked_add(Duration::from_millis(self.tick_ms))
                .unwrap_or(deadline);
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => {
                    self.service.begin_shutdown();
                    return Ok(self.outcome_from_current(124, true, false).await);
                }
                _ = interrupts.wait() => {
                    self.service.begin_shutdown();
                    return Ok(self.outcome_from_current(130, false, true).await);
                }
                _ = tokio::time::sleep_until(next_tick.min(deadline)) => {}
            }
        }
    }

    async fn outcome_from_current(
        &self,
        exit_code: u8,
        timed_out: bool,
        interrupted: bool,
    ) -> LocalOutcome {
        let snapshot = self.service.snapshot().await;
        self.outcome(&snapshot, exit_code, timed_out, interrupted)
    }

    fn outcome(
        &self,
        snapshot: &State,
        exit_code: u8,
        timed_out: bool,
        interrupted: bool,
    ) -> LocalOutcome {
        LocalOutcome {
            exit_code,
            report: local_report(
                snapshot,
                self.request_id,
                &self.idempotency_key,
                self.reused,
                timed_out,
                interrupted,
            ),
        }
    }
}

struct PreparedSubmission {
    lane_id: String,
    revision: String,
    components: Vec<String>,
    coverage_mode: CoverageMode,
    priority: Priority,
    deadline_at: Option<DateTime<Utc>>,
    reason: String,
    idempotency_key: String,
    request_hash: String,
    context: RequestContext,
    parameters: BTreeMap<String, String>,
}

fn select_lane(config: &Config, requested: Option<&str>) -> anyhow::Result<String> {
    let enabled = config
        .lanes
        .iter()
        .filter(|lane| lane.enabled)
        .collect::<Vec<_>>();
    if let Some(requested) = requested {
        return enabled
            .into_iter()
            .find(|lane| lane.id == requested)
            .map(|lane| lane.id.clone())
            .ok_or_else(|| LocalError::LaneNotEnabled.into());
    }
    match enabled.as_slice() {
        [lane] => Ok(lane.id.clone()),
        [] => Err(LocalError::NoEnabledLane.into()),
        _ => Err(LocalError::LaneSelectionRequired.into()),
    }
}

fn validate_submission(
    submission: &Submission,
    lane: &crate::config::LaneConfig,
) -> anyhow::Result<()> {
    if !is_full_revision(&submission.revision) {
        bail!("revision must be a full immutable Git object ID");
    }
    if submission.components.is_empty() || submission.components.len() > MAX_COMPONENTS {
        bail!("components must contain between 1 and 64 names");
    }
    let mut unique = BTreeSet::new();
    if submission.components.iter().any(|component| {
        component.is_empty()
            || component.len() > MAX_COMPONENT_BYTES
            || !component
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            || !unique.insert(component)
            || !lane.components.contains(component)
    }) {
        bail!("component names must be unique safe names allowed by the lane");
    }
    if submission.reason.len() > MAX_REASON_BYTES {
        bail!("reason exceeds 2 KiB");
    }
    if submission.coverage_mode == CoverageMode::Descendant && !lane.allow_descendant {
        bail!("descendant coverage is disabled for the selected lane");
    }
    if !submission.context.collaborators.is_empty() {
        bail!("local requests do not support collaborators");
    }
    for value in [&submission.context.task_id, &submission.context.session_id]
        .into_iter()
        .flatten()
    {
        if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            bail!(
                "task and session identifiers must be nonempty, bounded, and contain no control characters"
            );
        }
    }
    if let Some(key) = &submission.idempotency_key
        && (key.trim().is_empty()
            || key.len() > MAX_IDEMPOTENCY_KEY_BYTES
            || key.chars().any(char::is_control))
    {
        bail!(
            "idempotency key must be nonempty, at most 128 bytes, and contain no control characters"
        );
    }
    crate::domain::validate_parameters(&lane.request_parameters, &submission.parameters)
        .map_err(anyhow::Error::msg)?;
    Ok(())
}

fn prepare_submission(
    submission: Submission,
    lane_id: String,
) -> anyhow::Result<PreparedSubmission> {
    let mut components = submission.components.clone();
    components.sort();
    let mut body = json!({
        "lane_id": &lane_id,
        "revision": &submission.revision,
        "components": &components,
        "coverage_mode": &submission.coverage_mode,
        "priority": &submission.priority,
        "deadline_at": &submission.deadline_at,
        "reason": &submission.reason,
    });
    if submission.context.task_id.is_some() || !submission.context.collaborators.is_empty() {
        body["context"] = json!({
            "task_id": submission.context.task_id,
            "collaborators": submission.context.collaborators,
        });
    }
    if !submission.parameters.is_empty() {
        body["parameters"] = json!(submission.parameters);
    }
    let request_hash = hex::encode(Sha256::digest(
        serde_json::to_vec(&body).context("normalize local request")?,
    ));
    let idempotency_key = match submission.idempotency_key {
        Some(key) => key,
        None => match submission.context.task_id.as_deref() {
            Some(task_id) => {
                let key_material = serde_json::to_vec(&json!({
                    "task_id": task_id,
                    "request_hash": request_hash,
                }))
                .context("normalize local idempotency key")?;
                format!("local-task-{}", hex::encode(Sha256::digest(key_material)))
            }
            None => format!("local-{}", Uuid::new_v4().simple()),
        },
    };
    Ok(PreparedSubmission {
        lane_id,
        revision: submission.revision,
        components: submission.components,
        coverage_mode: submission.coverage_mode,
        priority: submission.priority,
        deadline_at: submission.deadline_at,
        reason: submission.reason,
        idempotency_key,
        request_hash,
        context: submission.context,
        parameters: submission.parameters,
    })
}

fn request_has_unknown_run(state: &State, request: &ReleaseRequest) -> bool {
    let lane_run = state
        .lanes
        .get(&request.lane_id)
        .and_then(|lane| lane.active_run_id);
    [request.current_run_id, lane_run]
        .into_iter()
        .flatten()
        .any(|id| {
            state
                .runs
                .get(&id)
                .is_some_and(|run| run.lane_id == request.lane_id && run.state == RunState::Unknown)
        })
}

fn result_code(state: &State, request_id: Uuid, observed_unknown: bool) -> Option<u8> {
    let request = state.requests.get(&request_id)?;
    if !request.status.is_terminal() && request_has_unknown_run(state, request) {
        return observed_unknown.then_some(2);
    }
    match &request.status {
        RequestStatus::Satisfied => Some(if satisfied_by_receipt(state, request).is_some() {
            0
        } else {
            2
        }),
        RequestStatus::Expired => Some(3),
        RequestStatus::Withdrawn => Some(4),
        RequestStatus::NeedsAttention => Some(2),
        RequestStatus::Accepted | RequestStatus::Waiting => None,
    }
}

fn run_receipt<'a>(state: &'a State, run: &'a DeliveryRun) -> Option<&'a DeploymentReceipt> {
    let receipt = state.receipts.get(&run.receipt_id?)?;
    (receipt.run_id == run.id
        && receipt.generation == run.generation
        && receipt.source_revision == run.target.revision
        && receipt.profile_digest == run.target.profile_digest
        && receipt
            .component_digests
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
            == run.target.components
        && receipt
            .component_digests
            .values()
            .all(|digest| !digest.trim().is_empty())
        && !receipt.verification_report.trim().is_empty()
        && receipt.observed_at >= run.created_at
        && receipt.observed_at <= Utc::now())
    .then_some(receipt)
}

fn satisfied_by_receipt<'a>(
    state: &'a State,
    request: &ReleaseRequest,
) -> Option<(&'a DeliveryRun, &'a DeploymentReceipt)> {
    if request.status != RequestStatus::Satisfied {
        return None;
    }
    let run = state.runs.get(&request.satisfied_by_run?)?;
    if run.state != RunState::Succeeded
        || run.lane_id != request.lane_id
        || run.target.repository_id != request.target.repository_id
        || run.target.profile_digest != request.target.profile_digest
        || run.target.release_line_id != request.target.release_line_id
        || run.target.config_version != request.target.config_version
        || run.target.parameters != request.target.parameters
        || !request.target.components.is_subset(&run.target.components)
        || (run.target.revision != request.target.revision
            && request.target.coverage_mode != CoverageMode::Descendant)
    {
        return None;
    }
    let receipt = state.receipts.get(&request.satisfied_by_receipt?)?;
    if run.receipt_id != Some(receipt.id)
        || receipt.result != ReceiptResult::Success
        || run_receipt(state, run)?.id != receipt.id
    {
        return None;
    }
    Some((run, receipt))
}

fn local_report(
    state: &State,
    request_id: Uuid,
    idempotency_key: &str,
    reused: bool,
    timed_out: bool,
    interrupted: bool,
) -> Value {
    let request = state.requests.get(&request_id);
    let satisfied = request.and_then(|request| satisfied_by_receipt(state, request));
    let run_id = request.and_then(|request| request.current_run_id.or(request.satisfied_by_run));
    let run = run_id.and_then(|id| state.runs.get(&id));
    let receipt = run.and_then(|run| run_receipt(state, run));
    let request_status = request.map(|request| {
        if request.status == RequestStatus::Satisfied && satisfied.is_none() {
            "needs_attention"
        } else {
            match &request.status {
                RequestStatus::Accepted => "accepted",
                RequestStatus::Waiting => "waiting",
                RequestStatus::Satisfied => "satisfied",
                RequestStatus::NeedsAttention => "needs_attention",
                RequestStatus::Withdrawn => "withdrawn",
                RequestStatus::Expired => "expired",
            }
        }
    });
    let request_value = request.map(|request| {
        json!({
            "id": request.id,
            "status": request_status,
            "lane_id": request.lane_id,
            "revision": request.target.revision,
            "components": request.target.components,
            "coverage_mode": request.target.coverage_mode,
            "priority": request.priority,
            "deadline_at": request.deadline_at,
            "created_at": request.created_at,
            "evidence": if request.status == RequestStatus::Satisfied && satisfied.is_none() {
                "satisfaction_receipt_missing_or_mismatched"
            } else if satisfied.is_some() {
                "matching_receipt"
            } else {
                "pending"
            },
        })
    });
    let run_value = run.map(|run| {
        json!({
            "id": run.id,
            "state": run.state,
            "lane_id": run.lane_id,
            "generation": run.generation,
            "phase": run.phase,
            "revision": run.target.revision,
            "components": run.target.components,
            "created_at": run.created_at,
            "updated_at": run.updated_at,
        })
    });
    let receipt_value = receipt.map(|receipt| {
        json!({
            "id": receipt.id,
            "run_id": receipt.run_id,
            "generation": receipt.generation,
            "result": receipt.result,
            "source_revision": receipt.source_revision,
            "components": receipt.component_digests.keys().collect::<Vec<_>>(),
            "observed_at": receipt.observed_at,
        })
    });
    json!({
        "schema_version": 1,
        "mode": "local",
        "request_id": request_id,
        "idempotency_key": idempotency_key,
        "reused": reused,
        "request": request_value,
        "run": run_value,
        "receipt": receipt_value,
        "local_wait_timeout": timed_out,
        "interrupted": interrupted,
        "scope": "local scheduler stopped; external execution may continue",
    })
}

type CtrlCSignal = Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>>;

struct InterruptReceiver {
    ctrl_c: CtrlCSignal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl InterruptReceiver {
    fn new() -> anyhow::Result<Self> {
        Ok(Self {
            ctrl_c: Box::pin(tokio::signal::ctrl_c()),
            #[cfg(unix)]
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("install SIGTERM listener")?,
        })
    }

    async fn wait(&mut self) {
        #[cfg(unix)]
        tokio::select! {
            _ = self.ctrl_c.as_mut() => {}
            _ = self.terminate.recv() => {}
        }
        #[cfg(not(unix))]
        {
            let _ = self.ctrl_c.as_mut().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LaneConfig, LaneExecutorConfig};

    fn git(path: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git fixture command failed");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn local_config(temp: &tempfile::TempDir) -> (Config, String) {
        let source = temp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let output = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg("--initial-branch=main")
            .arg(&source)
            .output()
            .unwrap();
        assert!(output.status.success(), "git init fixture failed");
        git(&source, &["config", "user.name", "Local Test"]);
        git(
            &source,
            &["config", "user.email", "local-test@example.invalid"],
        );
        std::fs::write(source.join("README"), "fixture\n").unwrap();
        git(&source, &["add", "README"]);
        git(&source, &["commit", "--quiet", "-m", "fixture"]);
        let revision = git(&source, &["rev-parse", "HEAD"]);
        let mirror = temp.path().join("source.git");
        let output = std::process::Command::new("git")
            .arg("clone")
            .arg("--bare")
            .arg("--quiet")
            .arg(&source)
            .arg(&mirror)
            .output()
            .unwrap();
        assert!(output.status.success(), "git bare clone fixture failed");
        (
            Config {
                mode: ConfigMode::Local,
                database_path: temp.path().join("ledger.sqlite3"),
                lanes: vec![LaneConfig {
                    id: "local-test".into(),
                    project: "fixture-project".into(),
                    environment: "test".into(),
                    repository_id: "fixture-repository".into(),
                    repository_path: mirror,
                    release_ref: "refs/heads/main".into(),
                    profile_digest: "fixture-profile".into(),
                    components: vec!["app".into()],
                    request_parameters: BTreeMap::new(),
                    allow_descendant: true,
                    allow_preemption: false,
                    enabled: true,
                    executor: LaneExecutorConfig::Scripted,
                    failure_observe: None,
                    resource_admission: None,
                }],
                scripted_phase_ms: 5,
                tick_ms: 1,
                ..Config::default()
            },
            revision,
        )
    }

    fn submission(revision: String, session_id: &str) -> Submission {
        Submission {
            lane_id: None,
            revision,
            components: vec!["app".into()],
            coverage_mode: CoverageMode::Exact,
            priority: Priority::Normal,
            deadline_at: None,
            reason: "local fixture".into(),
            idempotency_key: None,
            context: RequestContext {
                task_id: Some("local-task".into()),
                session_id: Some(session_id.into()),
                collaborators: BTreeSet::new(),
            },
            parameters: BTreeMap::new(),
        }
    }

    #[test]
    fn task_key_and_hash_ignore_session_and_component_order_is_normalized() {
        let one = Submission {
            revision: "a".repeat(40),
            components: vec!["worker".into(), "app".into()],
            ..submission("a".repeat(40), "session-one")
        };
        let two = Submission {
            revision: "a".repeat(40),
            components: vec!["app".into(), "worker".into()],
            ..submission("a".repeat(40), "session-two")
        };
        let first = prepare_submission(one, "lane".into()).unwrap();
        let second = prepare_submission(two, "lane".into()).unwrap();
        assert_eq!(first.request_hash, second.request_hash);
        assert_eq!(first.idempotency_key, second.idempotency_key);
        assert!(first.idempotency_key.starts_with("local-task-"));
    }

    #[tokio::test]
    async fn local_submit_replay_and_resume_keep_one_request_without_a_tick() {
        let temp = tempfile::tempdir().unwrap();
        let (config, revision) = local_config(&temp);
        let first = LocalSession::open(
            config.clone(),
            LocalAction::Submit(Box::new(submission(revision.clone(), "session-one"))),
        )
        .await
        .unwrap();
        let accepted = first.accepted();
        let request_id = accepted["request_id"].as_str().unwrap().parse().unwrap();
        assert_eq!(accepted["reused"], false);
        let first_outcome = first.wait(Duration::ZERO).await.unwrap();
        assert_eq!(first_outcome.exit_code, 124);
        assert_eq!(first_outcome.report["local_wait_timeout"], true);
        assert_eq!(first_outcome.report["run"], Value::Null);

        let replay = LocalSession::open(
            config.clone(),
            LocalAction::Submit(Box::new(submission(revision, "session-two"))),
        )
        .await
        .unwrap();
        let replay_accepted = replay.accepted();
        assert_eq!(replay_accepted["request_id"], accepted["request_id"]);
        assert_eq!(
            replay_accepted["idempotency_key"],
            accepted["idempotency_key"]
        );
        assert_eq!(replay_accepted["reused"], true);
        let _ = replay.wait(Duration::ZERO).await.unwrap();

        let resumed = LocalSession::open(
            config,
            LocalAction::Resume {
                request_id,
                retry: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(resumed.accepted()["request_id"], accepted["request_id"]);
        assert_eq!(resumed.accepted()["reused"], true);
        let resumed_outcome = resumed.wait(Duration::ZERO).await.unwrap();
        assert_eq!(resumed_outcome.exit_code, 124);
        assert_eq!(resumed_outcome.report["request"]["status"], "waiting");
    }

    #[tokio::test]
    async fn proven_failure_requires_explicit_retry_and_retains_the_request() {
        let temp = tempfile::tempdir().unwrap();
        let (mut config, revision) = local_config(&temp);
        config.coalesce_ms = 0;
        let first = LocalSession::open(
            config.clone(),
            LocalAction::Submit(Box::new(submission(revision, "first-session"))),
        )
        .await
        .unwrap();
        let request_id = first.request_id;
        first.service.scheduler_tick().await.unwrap();
        let snapshot = first.service.snapshot().await;
        let run = snapshot.runs.values().next().unwrap();
        let failed_run = run.id;
        first
            .service
            .executor_event(
                run.id,
                run.generation,
                crate::api::ExecutorEventKind::Failed,
                run.executor_ref.clone(),
                "synthetic known failure".into(),
                true,
            )
            .await
            .unwrap();
        assert_eq!(first.wait(Duration::ZERO).await.unwrap().exit_code, 2);

        let unchanged = LocalSession::open(
            config.clone(),
            LocalAction::Resume {
                request_id,
                retry: false,
            },
        )
        .await
        .unwrap();
        let state = unchanged.service.snapshot().await;
        assert_eq!(state.runs.len(), 1);
        assert!(state.lanes.values().next().unwrap().failure_paused);
        assert_eq!(unchanged.wait(Duration::ZERO).await.unwrap().exit_code, 2);

        let retry = LocalSession::open(
            config.clone(),
            LocalAction::Resume {
                request_id,
                retry: true,
            },
        )
        .await
        .unwrap();
        let completed = retry.wait(Duration::from_secs(10)).await.unwrap();
        assert_eq!(completed.exit_code, 0);
        assert_eq!(completed.report["request_id"], request_id.to_string());
        assert_ne!(completed.report["run"]["id"], failed_run.to_string());
        let service = Service::open(config).await.unwrap();
        let final_state = service.snapshot().await;
        assert_eq!(final_state.requests.len(), 1);
        assert_eq!(final_state.requests[&request_id].manual_retries_used, 1);
        assert_eq!(final_state.runs.len(), 2);
    }

    #[test]
    fn unknown_run_blocks_retry_and_satisfaction_requires_matching_receipt() {
        let request_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let lane = crate::domain::Lane {
            tenant_id: crate::domain::LOCAL_TENANT_ID.into(),
            id: "lane".into(),
            project: "project".into(),
            environment: "test".into(),
            enabled: true,
            repository_id: "repo".into(),
            release_ref: "refs/heads/main".into(),
            release_line_id: "line".into(),
            profile_digest: "profile".into(),
            config_version: 1,
            descendant_coverage_enabled: true,
            preemption_enabled: false,
            allowed_revisions: BTreeSet::new(),
            allowed_components: BTreeSet::from(["app".into()]),
            request_parameters: BTreeMap::new(),
            active_run_id: Some(run_id),
            generation: 1,
            blocked_reason: None,
            failure_paused: false,
        };
        let target = crate::domain::Target {
            repository_id: "repo".into(),
            revision: "a".repeat(40),
            components: BTreeSet::from(["app".into()]),
            profile_digest: "profile".into(),
            release_line_id: "line".into(),
            config_version: 1,
            coverage_mode: CoverageMode::Exact,
            parameters: BTreeMap::new(),
        };
        let request = ReleaseRequest {
            tenant_id: crate::domain::LOCAL_TENANT_ID.into(),
            context: RequestContext::default(),
            id: request_id,
            lane_id: "lane".into(),
            principal: LOCAL_PRINCIPAL.into(),
            idempotency_key: "key".into(),
            request_hash: "hash".into(),
            target: target.clone(),
            priority: Priority::Normal,
            deadline_at: None,
            status: RequestStatus::NeedsAttention,
            current_run_id: Some(run_id),
            satisfied_by_run: None,
            satisfied_by_receipt: None,
            reason: String::new(),
            waiting_reason: None,
            created_at: Utc::now(),
            creation_sequence: 1,
            auto_retries_used: 0,
            manual_retries_used: 0,
        };
        let run = DeliveryRun {
            id: run_id,
            lane_id: "lane".into(),
            target,
            effective_priority: Priority::Normal,
            state: RunState::Unknown,
            phase: crate::domain::Phase::Deploying,
            generation: 1,
            executor_ref: None,
            replacement_run_id: None,
            cancel_reason: None,
            receipt_id: None,
            resource_reservation: None,
            created_at: Utc::now(),
            phase_started_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let state = State {
            lanes: BTreeMap::from([("lane".into(), lane)]),
            requests: BTreeMap::from([(request_id, request.clone())]),
            runs: BTreeMap::from([(run_id, run)]),
            ..State::default()
        };
        assert!(request_has_unknown_run(&state, &request));
        assert_eq!(result_code(&state, request_id, false), None);
        assert_eq!(result_code(&state, request_id, true), Some(2));
        assert!(satisfied_by_receipt(&state, &request).is_none());

        // The request can still point at its old failed attempt while a
        // different request owns an unknown execution in the same lane.
        let mut mixed = state.clone();
        let old_id = Uuid::new_v4();
        let mut old_run = mixed.runs[&run_id].clone();
        old_run.id = old_id;
        old_run.state = RunState::Failed;
        mixed.runs.insert(old_id, old_run);
        mixed.requests.get_mut(&request_id).unwrap().current_run_id = Some(old_id);
        assert!(request_has_unknown_run(
            &mixed,
            &mixed.requests[&request_id]
        ));
        mixed.requests.get_mut(&request_id).unwrap().status = RequestStatus::Withdrawn;
        assert_eq!(result_code(&mixed, request_id, false), Some(4));
    }
}
