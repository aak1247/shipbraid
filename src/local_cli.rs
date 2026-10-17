use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::PathBuf,
    time::Duration,
};

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use clap::{Args, ValueEnum};
use serde_json::Value;
use shipbraid::{
    config::{Config, ConfigMode},
    domain::{CoverageMode, DomainError, Priority, RequestContext},
    lineage::is_full_revision,
    local::{LocalAction, LocalError, LocalSession, Submission},
    service::ServiceError,
};
use uuid::Uuid;

use crate::CliFailure;

#[derive(Clone, Debug, Args)]
pub struct RunArgs {
    /// Local coordinator TOML configuration.
    #[arg(long)]
    config: PathBuf,
    /// Select a lane when the local project has more than one enabled lane.
    #[arg(long, conflicts_with = "resume")]
    lane: Option<String>,
    /// Full Git commit object ID to submit.
    #[arg(long, conflicts_with = "resume", required_unless_present = "resume")]
    revision: Option<String>,
    /// Comma-separated components to submit.
    #[arg(
        long,
        value_delimiter = ',',
        conflicts_with = "resume",
        required_unless_present = "resume"
    )]
    components: Option<Vec<String>>,
    #[arg(long, value_enum, conflicts_with = "resume")]
    coverage: Option<CoverageArg>,
    #[arg(long, value_enum, conflicts_with = "resume")]
    priority: Option<PriorityArg>,
    /// Optional RFC3339 request deadline.
    #[arg(long, conflicts_with = "resume")]
    deadline: Option<String>,
    #[arg(long, conflicts_with = "resume")]
    reason: Option<String>,
    #[arg(long, conflicts_with = "resume")]
    idempotency_key: Option<String>,
    #[arg(long, conflicts_with = "resume")]
    task_id: Option<String>,
    #[arg(long, conflicts_with = "resume")]
    session_id: Option<String>,
    /// Frozen execution parameter, repeatable as KEY=VALUE.
    #[arg(long = "param", conflicts_with = "resume")]
    parameters: Vec<String>,
    /// Resume a persisted local request.
    #[arg(long, conflicts_with_all = [
        "lane",
        "revision",
        "components",
        "coverage",
        "priority",
        "deadline",
        "reason",
        "idempotency_key",
        "task_id",
        "session_id",
        "parameters"
    ])]
    resume: Option<Uuid>,
    /// Explicitly retry a request already proven failed.
    #[arg(long, requires = "resume")]
    retry: bool,
    /// Maximum local scheduler wait; zero accepts or resumes without a scheduler tick.
    #[arg(long, default_value_t = 300)]
    timeout_seconds: u64,
}

impl RunArgs {
    /// Validate submission-only arguments before loading configuration or
    /// opening any local state. Resume requests have no submission payload.
    pub fn validate(&self) -> Result<(), CliFailure> {
        if self.resume.is_some() {
            return Ok(());
        }
        submission(self).map(|_| ())
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CoverageArg {
    Exact,
    Descendant,
}

impl From<CoverageArg> for CoverageMode {
    fn from(value: CoverageArg) -> Self {
        match value {
            CoverageArg::Exact => Self::Exact,
            CoverageArg::Descendant => Self::Descendant,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PriorityArg {
    Normal,
    Urgent,
}

impl From<PriorityArg> for Priority {
    fn from(value: PriorityArg) -> Self {
        match value {
            PriorityArg::Normal => Self::Normal,
            PriorityArg::Urgent => Self::Urgent,
        }
    }
}

pub async fn run(args: &RunArgs, _json: bool) -> anyhow::Result<u8> {
    args.validate().map_err(anyhow::Error::new)?;
    let config = Config::load(&args.config)
        .map_err(|_| cli_error("local_config", "The local configuration is invalid."))?;
    if !matches!(config.mode, ConfigMode::Local) {
        return Err(cli_error(
            "wrong_mode",
            "The run command requires a local-mode configuration.",
        ));
    }

    let action = match args.resume {
        Some(request_id) => LocalAction::Resume {
            request_id,
            retry: args.retry,
        },
        None => LocalAction::Submit(Box::new(submission(args).map_err(anyhow::Error::new)?)),
    };
    let session = LocalSession::open(config, action)
        .await
        .map_err(|error| map_local_error(&error))?;
    let accepted = session.accepted();
    let request_id = accepted
        .get("request_id")
        .and_then(Value::as_str)
        .ok_or_else(|| cli_error("local_error", "The local request could not be identified."))?;
    let idempotency_key = accepted
        .get("idempotency_key")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            cli_error(
                "local_error",
                "The local request key could not be identified.",
            )
        })?;
    // Debug formatting keeps user-provided keys from injecting terminal lines.
    eprintln!("accepted request_id={request_id} idempotency_key={idempotency_key:?}");

    let outcome = session
        .wait(Duration::from_secs(args.timeout_seconds))
        .await
        .map_err(|error| map_local_error(&error))?;
    crate::output(&outcome.report)?;
    Ok(outcome.exit_code)
}

fn submission(args: &RunArgs) -> Result<Submission, CliFailure> {
    let revision = args
        .revision
        .clone()
        .ok_or_else(|| argument_error("invalid_arguments", "A full revision is required."))?;
    if !is_full_revision(&revision) {
        return Err(argument_error(
            "invalid_revision",
            "Revision must be a full Git object ID.",
        ));
    }
    let mut components = args.components.clone().ok_or_else(|| {
        argument_error("invalid_arguments", "At least one component is required.")
    })?;
    components.sort();
    components.dedup();
    if components.is_empty()
        || components
            .iter()
            .any(|component| component.trim().is_empty())
    {
        return Err(argument_error(
            "invalid_components",
            "Components must be nonempty comma-separated names.",
        ));
    }
    let deadline_at = args
        .deadline
        .as_deref()
        .map(|value| {
            DateTime::parse_from_rfc3339(value)
                .map(|time| time.with_timezone(&Utc))
                .map_err(|_| {
                    argument_error(
                        "invalid_deadline",
                        "Deadline must be a valid RFC3339 timestamp.",
                    )
                })
        })
        .transpose()?;
    let mut parameters = BTreeMap::new();
    for parameter in &args.parameters {
        let (key, value) = parameter
            .split_once('=')
            .ok_or_else(|| argument_error("invalid_parameter", "Use --param KEY=VALUE."))?;
        if key.is_empty()
            || value.is_empty()
            || parameters
                .insert(key.to_owned(), value.to_owned())
                .is_some()
        {
            return Err(argument_error(
                "invalid_parameter",
                "Parameter names must be unique and values nonempty.",
            ));
        }
    }

    Ok(Submission {
        lane_id: args.lane.clone(),
        revision,
        components,
        coverage_mode: args.coverage.unwrap_or(CoverageArg::Exact).into(),
        priority: args.priority.unwrap_or(PriorityArg::Normal).into(),
        deadline_at,
        reason: args.reason.clone().unwrap_or_default(),
        idempotency_key: args.idempotency_key.clone(),
        context: RequestContext {
            task_id: args.task_id.clone(),
            session_id: args.session_id.clone(),
            collaborators: BTreeSet::new(),
        },
        parameters,
    })
}

fn cli_error(code: &str, message: &str) -> anyhow::Error {
    anyhow!(CliFailure::new(code, message, false))
}

fn argument_error(code: &str, message: &str) -> CliFailure {
    CliFailure::new(code, message, false)
}

fn map_local_error(error: &anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<LocalError>() {
        Some(LocalError::LaneSelectionRequired) => {
            return cli_error(
                "lane_required",
                "Select one enabled local lane with --lane.",
            );
        }
        Some(LocalError::NoEnabledLane) => {
            return cli_error(
                "no_enabled_lane",
                "Configure at least one enabled lane for local mode.",
            );
        }
        Some(LocalError::LaneNotEnabled) => {
            return cli_error(
                "lane_not_found",
                "Select an enabled local lane with --lane.",
            );
        }
        Some(LocalError::UnknownRetryNotAllowed) => {
            return cli_error(
                "unknown_retry_not_allowed",
                "Resolve the unknown execution state before retrying the request.",
            );
        }
        None => {}
    }
    let domain_error = error.downcast_ref::<DomainError>().or_else(|| {
        match error.downcast_ref::<ServiceError>() {
            Some(ServiceError::Domain(domain)) => Some(domain),
            _ => None,
        }
    });
    match domain_error {
        Some(DomainError::IdempotencyConflict) => {
            return cli_error(
                "idempotency_conflict",
                "The idempotency key already names a different request.",
            );
        }
        Some(DomainError::RequestNotFound) => {
            return cli_error("request_not_found", "The local request was not found.");
        }
        Some(DomainError::NotRetryable | DomainError::RetryLimitExceeded) => {
            return cli_error(
                "request_not_retryable",
                "The local request cannot be retried in its current state.",
            );
        }
        Some(DomainError::LaneNotFound(_)) => {
            return cli_error("lane_not_found", "The selected local lane was not found.");
        }
        _ => {}
    }
    if error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::WouldBlock)
    {
        return cli_error(
            "ledger_locked",
            "The local ledger is already owned by another process.",
        );
    }
    if matches!(
        error.downcast_ref::<ServiceError>(),
        Some(ServiceError::Storage(_))
    ) {
        return cli_error("local_storage_error", "The local ledger operation failed.");
    }
    cli_error("local_error", "The local request could not be completed.")
}
