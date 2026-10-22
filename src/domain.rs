use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub type Id = Uuid;

const MAX_REQUEST_PARAMETERS: usize = 16;
const MAX_PARAMETER_KEY_BYTES: usize = 64;
const MAX_PARAMETER_VALUE_BYTES: usize = 1024;
const MAX_PARAMETER_ALLOWED_VALUES: usize = 32;

pub const LOCAL_TENANT_ID: &str = "local";

pub fn default_local_tenant() -> String {
    LOCAL_TENANT_ID.to_owned()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ParameterSpec {
    #[serde(default = "default_required")]
    pub required: bool,
    #[serde(default)]
    pub allowed_values: Vec<String>,
}

impl Default for ParameterSpec {
    fn default() -> Self {
        Self {
            required: true,
            allowed_values: Vec::new(),
        }
    }
}

fn default_required() -> bool {
    true
}

/// Validate an administrator-defined request-parameter schema.
pub fn validate_parameter_specs(specs: &BTreeMap<String, ParameterSpec>) -> Result<(), String> {
    if specs.len() > MAX_REQUEST_PARAMETERS {
        return Err(format!(
            "at most {MAX_REQUEST_PARAMETERS} request parameters are allowed"
        ));
    }
    for (key, spec) in specs {
        if !valid_parameter_text(key, MAX_PARAMETER_KEY_BYTES) {
            return Err(
                "parameter keys must be nonempty, bounded, and contain no control characters"
                    .to_owned(),
            );
        }
        if spec.allowed_values.len() > MAX_PARAMETER_ALLOWED_VALUES {
            return Err(format!("parameter {key} has too many allowed values"));
        }
        let mut unique = std::collections::BTreeSet::new();
        for value in &spec.allowed_values {
            if !valid_parameter_text(value, MAX_PARAMETER_VALUE_BYTES) {
                return Err(format!(
                    "parameter {key} has an empty or oversized allowed value"
                ));
            }
            if !unique.insert(value) {
                return Err(format!("parameter {key} has duplicate allowed values"));
            }
        }
    }
    Ok(())
}

/// Validate untrusted string parameters against a lane's frozen schema.
pub fn validate_parameters(
    specs: &BTreeMap<String, ParameterSpec>,
    values: &BTreeMap<String, String>,
) -> Result<(), String> {
    validate_parameter_specs(specs)?;
    if values.len() > MAX_REQUEST_PARAMETERS {
        return Err(format!(
            "at most {MAX_REQUEST_PARAMETERS} request parameters are allowed"
        ));
    }
    for (key, value) in values {
        if !valid_parameter_text(key, MAX_PARAMETER_KEY_BYTES) {
            return Err(
                "parameter keys must be nonempty, bounded, and contain no control characters"
                    .to_owned(),
            );
        }
        let spec = specs
            .get(key)
            .ok_or_else(|| format!("unknown request parameter: {key}"))?;
        if !valid_parameter_text(value, MAX_PARAMETER_VALUE_BYTES) {
            return Err(format!("parameter {key} must be a nonempty bounded string"));
        }
        if !spec.allowed_values.is_empty() && !spec.allowed_values.contains(value) {
            return Err(format!("parameter {key} is outside its allowed values"));
        }
    }
    for (key, spec) in specs {
        if spec.required && !values.contains_key(key) {
            return Err(format!("required request parameter is missing: {key}"));
        }
    }
    Ok(())
}

fn valid_parameter_text(value: &str, max_bytes: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn deserialize_local_tenant<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let tenant = String::deserialize(deserializer)?;
    if tenant != LOCAL_TENANT_ID {
        return Err(serde::de::Error::custom(
            "only the local tenant is supported",
        ));
    }
    Ok(tenant)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestContext {
    pub task_id: Option<String>,
    pub session_id: Option<String>,
    #[serde(default)]
    pub collaborators: BTreeSet<String>,
}

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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lane {
    #[serde(
        default = "default_local_tenant",
        deserialize_with = "deserialize_local_tenant"
    )]
    pub tenant_id: String,
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub request_parameters: BTreeMap<String, ParameterSpec>,
    pub active_run_id: Option<Id>,
    pub generation: u64,
    pub blocked_reason: Option<String>,
    pub failure_paused: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseRequest {
    #[serde(
        default = "default_local_tenant",
        deserialize_with = "deserialize_local_tenant"
    )]
    pub tenant_id: String,
    #[serde(default)]
    pub context: RequestContext,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_reservation: Option<ResourceReservation>,
    pub created_at: DateTime<Utc>,
    pub phase_started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceReservation {
    pub root_id: String,
    pub bytes: u64,
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
    #[serde(default)]
    pub actor: Option<String>,
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
    pub context: RequestContext,
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

#[cfg(test)]
mod parameter_tests {
    use super::{Lane, ParameterSpec, Target, validate_parameters};
    use std::collections::BTreeMap;

    fn schema() -> BTreeMap<String, ParameterSpec> {
        BTreeMap::from([(
            "database_backup".to_owned(),
            ParameterSpec {
                required: true,
                allowed_values: vec!["include".to_owned(), "skip".to_owned()],
            },
        )])
    }

    #[test]
    fn parameter_schema_requires_known_nonempty_bounded_values() {
        let specs = schema();
        assert!(validate_parameters(&specs, &BTreeMap::new()).is_err());
        assert!(
            validate_parameters(
                &specs,
                &BTreeMap::from([("database_backup".into(), "automatic".into())]),
            )
            .is_err()
        );
        assert!(
            validate_parameters(
                &specs,
                &BTreeMap::from([
                    ("database_backup".into(), "include".into()),
                    ("untrusted".into(), "value".into()),
                ]),
            )
            .is_err()
        );
        assert!(
            validate_parameters(
                &specs,
                &BTreeMap::from([("database_backup".into(), " ".into())]),
            )
            .is_err()
        );
        assert!(
            validate_parameters(
                &specs,
                &BTreeMap::from([("database_backup".into(), "x".repeat(1025))]),
            )
            .is_err()
        );
        assert!(
            validate_parameters(&specs, &BTreeMap::from([("x".repeat(65), "value".into())]),)
                .is_err()
        );
        assert!(
            validate_parameters(
                &specs,
                &BTreeMap::from([("database_backup".into(), "include".into())]),
            )
            .is_ok()
        );
    }

    #[test]
    fn optional_parameter_can_be_omitted_and_schema_defaults_are_required() {
        let parsed: ParameterSpec = serde_json::from_str("{}").unwrap();
        assert!(parsed.required);
        assert!(parsed.allowed_values.is_empty());
        assert!(serde_json::from_str::<ParameterSpec>(r#"{"required":false,"extra":1}"#).is_err());
        let optional = BTreeMap::from([(
            "note".to_owned(),
            ParameterSpec {
                required: false,
                allowed_values: Vec::new(),
            },
        )]);
        assert!(validate_parameters(&optional, &BTreeMap::new()).is_ok());
    }

    #[test]
    fn old_lane_and_target_json_without_parameter_fields_remains_compatible() {
        let mut lane_json = serde_json::json!({
            "id": "test", "project": "p", "environment": "dev", "enabled": true,
            "repository_id": "repo", "release_ref": "refs/heads/main",
            "release_line_id": "stable", "profile_digest": "profile",
            "config_version": 1, "descendant_coverage_enabled": false,
            "preemption_enabled": false, "allowed_revisions": [],
            "allowed_components": ["app"], "active_run_id": null, "generation": 0,
            "blocked_reason": null, "failure_paused": false
        });
        lane_json.as_object_mut().unwrap().remove("tenant_id");
        let lane: Lane = serde_json::from_value(lane_json).unwrap();
        assert!(lane.request_parameters.is_empty());
        assert!(
            serde_json::to_value(&lane)
                .unwrap()
                .get("request_parameters")
                .is_none()
        );

        let target: Target = serde_json::from_value(serde_json::json!({
            "repository_id": "repo", "revision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "components": ["app"], "profile_digest": "profile",
            "release_line_id": "stable", "config_version": 1,
            "coverage_mode": "exact"
        }))
        .unwrap();
        assert!(target.parameters.is_empty());
        assert!(
            serde_json::to_value(&target)
                .unwrap()
                .get("parameters")
                .is_none()
        );
    }
}
