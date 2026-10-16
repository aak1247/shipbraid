use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub type Id = Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum CoverageMode {
    #[default]
    Exact,
    Descendant,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Normal,
    Urgent,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Accepted,
    Waiting,
    Satisfied,
    NeedsAttention,
    Withdrawn,
    Expired,
}

impl RequestStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Satisfied | Self::Withdrawn | Self::Expired)
    }

    pub fn can_be_scheduled(&self) -> bool {
        matches!(self, Self::Accepted | Self::Waiting | Self::NeedsAttention)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Planned,
    Dispatching,
    Running,
    CancelRequested,
    Superseded,
    Canceled,
    Unknown,
    Succeeded,
    Failed,
}

impl RunState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Superseded | Self::Canceled | Self::Succeeded | Self::Failed
        )
    }

    pub fn owns_execution_slot(&self) -> bool {
        matches!(
            self,
            Self::Dispatching | Self::Running | Self::CancelRequested | Self::Unknown
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Preparing,
    Building,
    Deploying,
    Verifying,
}

impl Phase {
    pub fn is_preemptible(&self) -> bool {
        matches!(self, Self::Preparing | Self::Building)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Target {
    pub repository_id: String,
    pub revision: String,
    pub components: BTreeSet<String>,
    pub profile_digest: String,
    pub release_line_id: String,
    pub config_version: u64,
    pub coverage_mode: CoverageMode,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lane {
    pub id: String,
    pub project: String,
    pub environment: String,
    pub enabled: bool,
    pub repository_id: String,
    pub release_ref: String,
    pub release_line_id: String,
    pub profile_digest: String,
    pub config_version: u64,
    pub descendant_coverage_enabled: bool,
    pub preemption_enabled: bool,
    pub allowed_revisions: BTreeSet<String>,
    pub allowed_components: BTreeSet<String>,
    pub active_run_id: Option<Id>,
    pub generation: u64,
    pub blocked_reason: Option<String>,
    pub failure_paused: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseRequest {
    pub id: Id,
    pub lane_id: String,
    pub principal: String,
    pub idempotency_key: String,
    pub request_hash: String,
    pub target: Target,
    pub priority: Priority,
    pub deadline_at: Option<DateTime<Utc>>,
    pub status: RequestStatus,
    pub current_run_id: Option<Id>,
    pub satisfied_by_run: Option<Id>,
    pub satisfied_by_receipt: Option<Id>,
    pub reason: String,
    pub waiting_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub creation_sequence: u64,
    pub auto_retries_used: u32,
    pub manual_retries_used: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeliveryRun {
    pub id: Id,
    pub lane_id: String,
    pub target: Target,
    pub effective_priority: Priority,
    pub state: RunState,
    pub phase: Phase,
    pub generation: u64,
    pub executor_ref: Option<String>,
    pub replacement_run_id: Option<Id>,
    pub cancel_reason: Option<String>,
    pub receipt_id: Option<Id>,
    pub created_at: DateTime<Utc>,
    pub phase_started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptResult {
    Success,
    Failure,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentReceipt {
    pub id: Id,
    pub run_id: Id,
    pub generation: u64,
    pub observed_at: DateTime<Utc>,
    pub source_revision: String,
    pub component_digests: BTreeMap<String, String>,
    pub profile_digest: String,
    pub verification_report: String,
    pub result: ReceiptResult,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    Dispatch,
    Cancel,
    Observe,
    Verify,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandDeliveryState {
    Pending,
    Delivered,
    Acknowledged,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Command {
    pub id: Id,
    pub run_id: Id,
    pub generation: u64,
    pub kind: CommandKind,
    pub delivery_state: CommandDeliveryState,
    pub payload_digest: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    pub sequence: u64,
    pub lane_id: String,
    pub run_id: Option<Id>,
    pub request_id: Option<Id>,
    pub timestamp: DateTime<Utc>,
    pub kind: String,
    pub message: String,
}

/// A serializable in-memory projection of the relational ledger.
///
/// Persistence adapters must continue storing each entity in its own table;
/// this type is intended for transaction snapshots and deterministic rules.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    pub lanes: BTreeMap<String, Lane>,
    pub requests: BTreeMap<Id, ReleaseRequest>,
    pub runs: BTreeMap<Id, DeliveryRun>,
    pub receipts: BTreeMap<Id, DeploymentReceipt>,
    pub commands: BTreeMap<Id, Command>,
    pub events: Vec<Event>,
    pub next_event_sequence: BTreeMap<String, u64>,
    pub draining: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitInput {
    pub id: Id,
    pub principal: String,
    pub idempotency_key: String,
    pub request_hash: String,
    pub lane_id: String,
    pub target: Target,
    pub authorization: TargetAuthorization,
    pub priority: Priority,
    pub deadline_at: Option<DateTime<Utc>>,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TargetAuthorization {
    pub repository_id: String,
    pub release_ref: String,
    pub release_tip: String,
    pub revision: String,
    pub release_line_id: String,
    pub config_version: u64,
    pub relation: LineageRelation,
    pub proof_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LineageRelation {
    Ancestor,
    NotAncestor,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnknownResolution {
    KnownRunning {
        executor_ref: String,
        evidence_digest: String,
    },
    KnownStopped {
        executor_ref: Option<String>,
        evidence_digest: String,
    },
    VerifiedReceipt {
        receipt: DeploymentReceipt,
        evidence_digest: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitOutcome {
    pub request_id: Id,
    pub reused: bool,
    pub attached_run_id: Option<Id>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileIds {
    pub run_id: Id,
    pub command_id: Id,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileOutcome {
    Idle,
    WaitingForMergeWindow {
        request_id: Id,
        until: DateTime<Utc>,
    },
    Blocked {
        reason: String,
    },
    PermitUnavailable,
    Dispatched {
        run_id: Id,
        command_id: Id,
        attached_requests: Vec<Id>,
    },
    PreemptionRequested {
        old_run_id: Id,
        new_run_id: Id,
        cancel_command_id: Id,
    },
    CancelRequested {
        run_id: Id,
        command_id: Id,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandDelivery {
    Delivered,
    Acknowledged,
    Unknown { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutorResult {
    Started {
        executor_ref: String,
    },
    Stopped {
        executor_ref: String,
    },
    Failed {
        message: String,
        environment_known_unchanged: bool,
    },
    Unknown {
        reason: String,
    },
    VerifiedReceipt(DeploymentReceipt),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("lane does not exist: {0}")]
    LaneNotFound(String),
    #[error("request does not exist")]
    RequestNotFound,
    #[error("run does not exist")]
    RunNotFound,
    #[error("command does not exist")]
    CommandNotFound,
    #[error("idempotency key was reused with a different request body")]
    IdempotencyConflict,
    #[error("target is not allowed: {0}")]
    TargetNotAllowed(String),
    #[error("request deadline has elapsed")]
    DeadlineElapsed,
    #[error("request belongs to a different principal")]
    RequestNotOwned,
    #[error("request is already terminal")]
    RequestTerminal,
    #[error("request cannot be retried in its current state")]
    NotRetryable,
    #[error("manual retry limit reached")]
    RetryLimitExceeded,
    #[error("lane is blocked: {0}")]
    LaneBlocked(String),
    #[error("lineage evidence is missing or does not match the requested revisions")]
    LineageUnproven,
    #[error("transition is invalid for the current run state")]
    InvalidTransition,
    #[error("run generation does not match the active lane generation")]
    StaleGeneration,
    #[error("executor reference does not match the recorded run")]
    ExecutorMismatch,
    #[error("receipt does not match the run target or is not verified")]
    InvalidReceipt,
    #[error("run ownership is not proven stopped")]
    OwnershipUnproven,
    #[error("identifier already exists")]
    DuplicateId,
}
