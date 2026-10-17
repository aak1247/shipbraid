use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use reqwest::{Client, Method};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(version, about = "发布请求协调器")]
struct Cli {
    /// Emit structured JSON errors and doctor reports; API successes are JSON.
    #[arg(long, global = true)]
    json: bool,
    #[arg(
        long,
        global = true,
        env = "SHIPBRAID_URL",
        default_value = "http://127.0.0.1:8787"
    )]
    url: String,
    #[arg(long, global = true, env = "SHIPBRAID_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[arg(
        long,
        global = true,
        env = "SHIPBRAID_TOKEN_FILE",
        conflicts_with = "token"
    )]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Check local prerequisites without opening the ledger or executing hooks.
    Doctor {
        /// Local coordinator TOML configuration, resolved like `serve --config`.
        #[arg(long)]
        config: PathBuf,
        /// Also check the local adapter's user systemd and cgroup prerequisites.
        #[arg(long)]
        check_local_runtime: bool,
    },
    /// Start the durable coordinator with configured executors.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// Submit an immutable release request; retries must reuse the same key.
    Submit {
        #[arg(long, required_unless_present = "project", conflicts_with_all = ["project", "environment"])]
        lane: Option<String>,
        #[arg(long, requires = "environment", required_unless_present = "lane")]
        project: Option<String>,
        #[arg(long, requires = "project")]
        environment: Option<String>,
        #[arg(long)]
        revision: String,
        #[arg(long, value_delimiter = ',', required = true)]
        components: Vec<String>,
        #[arg(long, value_enum, default_value = "exact")]
        coverage: Coverage,
        #[arg(long, value_enum, default_value = "normal")]
        priority: Priority,
        #[arg(long)]
        deadline: Option<String>,
        #[arg(long, default_value = "")]
        reason: String,
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Stable development task reference, shared across collaborating agents.
        #[arg(long)]
        task_id: Option<String>,
        /// Creation-session trace only; does not change idempotency semantics.
        #[arg(long)]
        session_id: Option<String>,
        /// Configured principal allowed to manage this request (repeatable).
        #[arg(long = "collaborator")]
        collaborators: Vec<String>,
        /// Frozen non-secret execution parameter (repeatable KEY=VALUE).
        #[arg(long = "param")]
        parameters: Vec<String>,
    },
    /// Wait locally; timeout/disconnect does not withdraw the server request.
    Wait {
        request_id: String,
        #[arg(long, default_value_t = 300)]
        timeout_seconds: u64,
        #[arg(long, default_value_t = 500)]
        poll_ms: u64,
    },
    Inspect {
        #[arg(value_enum)]
        kind: Resource,
        id: String,
    },
    List {
        #[arg(value_enum)]
        kind: Resource,
        #[arg(long)]
        lane: Option<String>,
        #[arg(long)]
        task_id: Option<String>,
        #[arg(long)]
        principal: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..=200))]
        limit: Option<u16>,
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Discover the authenticated principal, tenant, and API capabilities.
    System,
    Events {
        #[arg(long)]
        lane: String,
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
    /// Query a fixed run or the request's current run.
    #[command(subcommand_negates_reqs = true)]
    Logs {
        /// Legacy shorthand for `logs run RUN_ID`.
        #[arg(required = true)]
        run_id: Option<String>,
        /// Numeric sequence cursor for the legacy run-log shorthand.
        #[arg(long, requires = "run_id")]
        after: Option<u64>,
        #[command(subcommand)]
        target: Option<LogTarget>,
    },
    Withdraw {
        request_id: String,
    },
    Retry {
        request_id: String,
    },
    Priority {
        request_id: String,
        #[arg(value_enum)]
        priority: Priority,
        #[arg(long, default_value = "CLI priority update")]
        reason: String,
    },
    /// Stop accepting new dispatches while observing active runs.
    Drain,
    /// Resume dispatch after an explicit drain.
    Resume,
    /// Resolve unknown only with independently observed executor evidence.
    ResolveUnknown {
        #[arg(long)]
        lane: String,
        #[arg(long, value_enum)]
        action: Resolution,
        #[arg(long)]
        evidence: String,
        #[arg(long)]
        reason: String,
    },
    /// Check process readiness without credentials.
    Health,
}
#[derive(Subcommand)]
enum LogTarget {
    /// Query one run's logs using its numeric sequence cursor.
    Run {
        run_id: String,
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
    /// Query the current run; a saved cursor resets when the request changes runs.
    Request {
        request_id: String,
        /// Opaque next_cursor from the preceding request-log response.
        #[arg(long)]
        cursor: Option<String>,
    },
}
#[derive(Clone, ValueEnum)]
enum Coverage {
    Exact,
    Descendant,
}
#[derive(Clone, ValueEnum)]
enum Priority {
    Normal,
    Urgent,
}
#[derive(Clone, ValueEnum)]
enum Resolution {
    ConfirmStopped,
    AdoptRunning,
}
#[derive(Clone, ValueEnum)]
enum Resource {
    Request,
    Run,
    Lane,
}
impl Resource {
    fn path(&self) -> &'static str {
        match self {
            Self::Request => "requests",
            Self::Run => "runs",
            Self::Lane => "lanes",
        }
    }
}
impl Priority {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Urgent => "urgent",
        }
    }
}
impl Coverage {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Descendant => "descendant",
        }
    }
}
#[derive(Debug, Serialize)]
struct CliFailure {
    code: String,
    message: String,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    idempotency_key: Option<String>,
}
impl CliFailure {
    fn new(code: &str, message: &str, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
            http_status: None,
            retry_after_seconds: None,
            idempotency_key: None,
        }
    }
}
impl Cli {
    fn validate(&self) -> std::result::Result<(), CliFailure> {
        match &self.command {
            Command::Logs {
                run_id,
                after,
                target: Some(_),
            } if run_id.is_some() || after.is_some() => Err(CliFailure::new(
                "invalid_arguments",
                "Use either legacy logs syntax or an explicit logs subcommand, not both.",
                false,
            )),
            Command::Logs {
                run_id: None,
                target: None,
                ..
            } => Err(CliFailure::new(
                "invalid_arguments",
                "Specify a run ID or an explicit logs subcommand.",
                false,
            )),
            _ => Ok(()),
        }
    }
}
#[derive(Debug)]
enum CliParseError {
    Clap(clap::Error),
    Validation(CliFailure),
}
fn parse_cli<I, T>(arguments: I) -> std::result::Result<Cli, CliParseError>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let cli = Cli::try_parse_from(arguments).map_err(CliParseError::Clap)?;
    cli.validate().map_err(CliParseError::Validation)?;
    Ok(cli)
}
impl std::fmt::Display for CliFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for CliFailure {}

fn failure_value(error: &anyhow::Error) -> Value {
    match error.downcast_ref::<CliFailure>() {
        Some(failure) => json!({"error": failure}),
        // Never serialize arbitrary error chains: they may contain credentials,
        // a client-supplied URL, a response body, or a private filesystem path.
        None => json!({"error": CliFailure::new(
            "local_error",
            "The command could not be completed. Check arguments, token file, and local configuration.",
            false,
        )}),
    }
}

struct Api {
    client: Client,
    base: reqwest::Url,
    token: Option<String>,
}
impl Api {
    fn new(url: &str, token: Option<String>) -> Result<Self> {
        let mut base = reqwest::Url::parse(url).context("invalid server URL")?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || (base.scheme() == "http"
                && !matches!(
                    base.host_str(),
                    Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
                ))
        {
            bail!(
                "server URL must use HTTPS (or loopback HTTP), without credentials, query or fragment"
            );
        }
        base.set_path(&format!("{}/", base.path().trim_end_matches('/')));
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            base,
            token,
        })
    }
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        key: Option<&str>,
    ) -> Result<Value> {
        self.call_with_timeout(method, path, body, key, None).await
    }
    async fn call_with_timeout(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        key: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        let url = self.base.join(path.trim_start_matches('/'))?;
        let mut request = self.client.request(method, url);
        if path.starts_with("/v1/") {
            let token = self
                .token
                .as_deref()
                .context("set SHIPBRAID_TOKEN or --token")?;
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        if let Some(key) = key {
            request = request.header("Idempotency-Key", key);
        }
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        let response = request.send().await.map_err(|_| CliFailure::new(
            "transport_error", "The coordinator response was not received. The operation may have been accepted; recover using the original request or idempotency key.", true,
        ))?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let value: Value = response.json().await.map_err(|_| {
            let mut failure = CliFailure::new(
                "invalid_response", "The coordinator returned an unreadable JSON response. Recover using the original request or idempotency key.", status.is_success() || status.is_server_error() || status.as_u16() == 429,
            );
            failure.http_status = Some(status.as_u16());
            failure.retry_after_seconds = retry_after;
            failure
        })?;
        if !status.is_success() {
            let code = value
                .get("code")
                .and_then(Value::as_str)
                .filter(|code| {
                    !code.is_empty()
                        && code.len() <= 64
                        && code
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                })
                .unwrap_or("http_error");
            let mut message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("The coordinator rejected the operation.")
                .to_owned();
            if let Some(token) = self.token.as_deref().filter(|token| !token.is_empty()) {
                message = message.replace(token, "[redacted]");
            }
            message = message.replace(self.base.as_str(), "[coordinator]");
            message = message
                .chars()
                .filter(|c| !c.is_control())
                .take(2048)
                .collect();
            let mut failure = CliFailure::new(
                code,
                &message,
                value
                    .get("retryable")
                    .and_then(Value::as_bool)
                    .unwrap_or(status.is_server_error() || status.as_u16() == 429),
            );
            failure.http_status = Some(status.as_u16());
            failure.retry_after_seconds = retry_after;
            return Err(failure.into());
        }
        Ok(value)
    }
    async fn get(&self, path: &str) -> Result<Value> {
        self.call(Method::GET, path, None, None).await
    }
    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.call(Method::POST, path, Some(body), None).await
    }
}

async fn canonical_task_intent(api: &Api, body: &Value) -> Result<Value> {
    let mut intent = body.clone();
    if intent.get("lane_id").is_none() {
        let mut url = api.base.join("v1/lanes")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair(
                "project",
                body["project"].as_str().context("project is required")?,
            );
            query.append_pair(
                "environment",
                body["environment"]
                    .as_str()
                    .context("environment is required")?,
            );
        }
        let path = format!("/v1/lanes?{}", url.query().unwrap_or_default());
        let lanes = api.get(&path).await?;
        let items = lanes
            .get("items")
            .and_then(Value::as_array)
            .filter(|items| items.len() == 1)
            .context("target must resolve to one authorized lane")?;
        let id = items[0]
            .get("id")
            .and_then(Value::as_str)
            .context("lane has no identity")?;
        intent["lane_id"] = json!(id);
    }
    if let Some(object) = intent.as_object_mut() {
        object.remove("project");
        object.remove("environment");
    }
    Ok(intent)
}

fn task_idempotency_key(body: &Value) -> String {
    let mut intent = body.clone();
    if let Some(object) = intent.as_object_mut() {
        object.remove("session_id");
    }
    format!(
        "task-{}",
        hex::encode(Sha256::digest(
            serde_json::to_vec(&intent).expect("JSON intent is serializable")
        ))
    )
}
fn segment(value: &str) -> Result<&str> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("invalid resource identifier");
    }
    Ok(value)
}
fn run_logs_path(run_id: &str, after: u64) -> Result<String> {
    Ok(format!(
        "/v1/runs/{}/logs?after={after}&limit=200",
        segment(run_id)?
    ))
}
fn request_logs_path(
    base: &reqwest::Url,
    request_id: &str,
    cursor: Option<&str>,
) -> Result<String> {
    let path = format!("/v1/requests/{}/logs", segment(request_id)?);
    let mut url = base.join(path.trim_start_matches('/'))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("limit", "200");
        if let Some(cursor) = cursor {
            query.append_pair("cursor", cursor);
        }
    }
    Ok(format!("{path}?{}", url.query().unwrap_or_default()))
}
fn output(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "shipbraid=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let arguments: Vec<_> = std::env::args_os().collect();
    let machine_mode = arguments.iter().any(|argument| argument == "--json");
    let cli = match parse_cli(arguments) {
        Ok(cli) => cli,
        Err(CliParseError::Validation(failure)) => {
            if machine_mode {
                let _ = output(&json!({"error": failure}));
            } else {
                eprintln!("{failure}");
            }
            return std::process::ExitCode::from(2);
        }
        Err(CliParseError::Clap(error)) => {
            if machine_mode
                && !matches!(
                    error.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                )
            {
                let failure = CliFailure::new(
                    "invalid_arguments",
                    "Invalid command arguments. Use --help for the command contract.",
                    false,
                );
                let _ = output(&json!({"error": failure}));
                return std::process::ExitCode::from(2);
            }
            let code = error.exit_code() as u8;
            let _ = error.print();
            return std::process::ExitCode::from(code);
        }
    };
    match run(cli).await {
        Ok(code) => std::process::ExitCode::from(code),
        Err(error) => {
            if machine_mode {
                let _ = output(&failure_value(&error));
            } else {
                eprintln!("{error:#}");
            }
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<u8> {
    cli.validate()?;
    if let Command::Doctor {
        config,
        check_local_runtime,
    } = &cli.command
    {
        let report = shipbraid::diagnostics::diagnose(config, *check_local_runtime).await;
        let code = if report.ok { 0 } else { 1 };
        if cli.json {
            output(&serde_json::to_value(report)?)?;
        } else {
            println!("{}", report.to_text());
        }
        return Ok(code);
    }
    if let Command::Serve { config } = &cli.command {
        shipbraid::service::serve(shipbraid::config::Config::load(config)?).await?;
        return Ok(0);
    }
    let token = match cli.token_file {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .context("cannot read token file")?
                .trim()
                .to_owned(),
        ),
        None => cli.token,
    };
    let api = Api::new(&cli.url, token)?;
    let value = match cli.command {
        Command::Serve { .. } | Command::Doctor { .. } => unreachable!(),
        Command::Submit {
            lane,
            project,
            environment,
            revision,
            components,
            coverage,
            priority,
            deadline,
            reason,
            idempotency_key,
            task_id,
            session_id,
            mut collaborators,
            parameters,
        } => {
            let mut components = components;
            components.sort();
            components.dedup();
            collaborators.sort();
            collaborators.dedup();
            let deadline = deadline
                .map(|value| {
                    chrono::DateTime::parse_from_rfc3339(&value).map(|time| {
                        time.with_timezone(&chrono::Utc)
                            .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
                    })
                })
                .transpose()
                .map_err(|_| {
                    CliFailure::new(
                        "invalid_deadline",
                        "deadline must be a valid RFC3339 timestamp.",
                        false,
                    )
                })?;
            let mut body = json!({"revision":revision,"components":components,"coverage_mode":coverage.as_str(),"priority":priority.as_str(),"deadline_at":deadline,"reason":reason});
            let mut frozen_parameters = std::collections::BTreeMap::new();
            for parameter in parameters {
                let (key, value) = parameter.split_once('=').ok_or_else(|| {
                    CliFailure::new("invalid_parameter", "Use --param KEY=VALUE.", false)
                })?;
                if key.is_empty()
                    || value.is_empty()
                    || frozen_parameters
                        .insert(key.to_owned(), value.to_owned())
                        .is_some()
                {
                    return Err(CliFailure::new(
                        "invalid_parameter",
                        "Parameter names must be unique and values nonempty.",
                        false,
                    )
                    .into());
                }
            }
            if !frozen_parameters.is_empty() {
                body["parameters"] = json!(frozen_parameters);
            }
            if let Some(task_id) = &task_id {
                body["task_id"] = json!(task_id);
            }
            if let Some(session_id) = session_id {
                body["session_id"] = json!(session_id);
            }
            if !collaborators.is_empty() {
                body["collaborators"] = json!(collaborators);
            }
            if let Some(lane) = lane {
                body["lane_id"] = lane.into();
            } else {
                body["project"] = project.context("project is required without lane")?.into();
                body["environment"] = environment
                    .context("environment is required without lane")?
                    .into();
            }
            let key = if let Some(key) = idempotency_key {
                key
            } else if task_id.is_some() {
                task_idempotency_key(&canonical_task_intent(&api, &body).await?)
            } else {
                uuid::Uuid::new_v4().to_string()
            };
            if !cli.json {
                eprintln!("idempotency-key: {key}");
            }
            let mut result = match api
                .call(Method::POST, "/v1/requests", Some(body), Some(&key))
                .await
            {
                Ok(value) => value,
                Err(mut error) => {
                    if let Some(failure) = error.downcast_mut::<CliFailure>() {
                        failure.idempotency_key = Some(key);
                    }
                    return Err(error);
                }
            };
            if let Some(object) = result.as_object_mut() {
                object.insert("idempotency_key".into(), json!(key));
            }
            result
        }
        Command::Wait {
            request_id,
            timeout_seconds,
            poll_ms,
        } => {
            if poll_ms == 0 {
                bail!("poll-ms must be positive");
            }
            let path = format!("/v1/requests/{}", segment(&request_id)?);
            let started = Instant::now();
            let budget = Duration::from_secs(timeout_seconds);
            let mut last_observation = None;
            let mut failures = 0u32;
            loop {
                let remaining = budget.saturating_sub(started.elapsed());
                // A zero-second wait still performs one bounded observation.
                let query_timeout = remaining
                    .min(Duration::from_secs(15))
                    .max(Duration::from_millis(100));
                match api
                    .call_with_timeout(Method::GET, &path, None, None, Some(query_timeout))
                    .await
                {
                    Ok(value) => {
                        failures = 0;
                        let code = match value.get("status").and_then(Value::as_str) {
                            Some("satisfied") => Some(0),
                            Some("needs_attention") => Some(2),
                            Some("expired") => Some(3),
                            Some("withdrawn") => Some(4),
                            _ => None,
                        };
                        if let Some(code) = code {
                            output(&value)?;
                            return Ok(code);
                        }
                        last_observation = Some(value);
                    }
                    Err(error) => {
                        if !error
                            .downcast_ref::<CliFailure>()
                            .is_some_and(|failure| failure.retryable)
                        {
                            return Err(error);
                        }
                        failures = failures.saturating_add(1);
                        let failure = error
                            .downcast_ref::<CliFailure>()
                            .expect("checked structured failure");
                        let backoff = Duration::from_millis(poll_ms.max(100))
                            .saturating_mul(1u32 << failures.min(5))
                            .min(Duration::from_secs(5))
                            .max(Duration::from_secs(
                                failure.retry_after_seconds.unwrap_or_default(),
                            ));
                        let remaining = budget.saturating_sub(started.elapsed());
                        if !remaining.is_zero() {
                            tokio::time::sleep(backoff.min(remaining)).await;
                        }
                    }
                }
                let remaining = budget.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    let mut value = last_observation.unwrap_or_else(
                        || json!({"request_id": request_id, "observation_available": false}),
                    );
                    value["local_wait_timeout"] = json!(true);
                    output(&value)?;
                    if !cli.json {
                        eprintln!("local wait timeout; server request remains active");
                    }
                    return Ok(124);
                }
                if failures == 0 {
                    tokio::time::sleep(Duration::from_millis(poll_ms).min(remaining)).await;
                }
            }
        }

        Command::Inspect { kind, id } => {
            api.get(&format!("/v1/{}/{}", kind.path(), segment(&id)?))
                .await?
        }
        Command::List {
            kind,
            lane,
            task_id,
            principal,
            status,
            limit,
            cursor,
        } => {
            if !matches!(kind, Resource::Request)
                && [
                    task_id.as_ref(),
                    principal.as_ref(),
                    status.as_ref(),
                    cursor.as_ref(),
                ]
                .iter()
                .any(|value| value.is_some())
            {
                bail!("task/principal/status/cursor filters apply only to request lists");
            }
            let mut url = api.base.join(&format!("v1/{}", kind.path()))?;
            {
                let mut query = url.query_pairs_mut();
                if let Some(lane) = lane {
                    query.append_pair("lane_id", segment(&lane)?);
                }
                if let Some(task_id) = task_id {
                    query.append_pair("task_id", &task_id);
                }
                if let Some(principal) = principal {
                    query.append_pair("principal", &principal);
                }
                if let Some(status) = status {
                    query.append_pair("status", &status);
                }
                if let Some(limit) = limit {
                    query.append_pair("limit", &limit.to_string());
                }
                if let Some(cursor) = cursor {
                    query.append_pair("cursor", &cursor);
                }
            }
            let path = format!(
                "/v1/{}{}",
                kind.path(),
                url.query()
                    .map(|query| format!("?{query}"))
                    .unwrap_or_default()
            );
            api.get(&path).await?
        }
        Command::System => api.get("/v1/system").await?,
        Command::Events { lane, after } => {
            let path = format!(
                "/v1/events?after={after}&limit=200&lane_id={}",
                segment(&lane)?
            );
            api.get(&path).await?
        }
        Command::Logs {
            run_id,
            after,
            target,
        } => match target {
            Some(LogTarget::Run { run_id, after }) => {
                api.get(&run_logs_path(&run_id, after)?).await?
            }
            Some(LogTarget::Request { request_id, cursor }) => {
                api.get(&request_logs_path(
                    &api.base,
                    &request_id,
                    cursor.as_deref(),
                )?)
                .await?
            }
            None => {
                let run_id = run_id.context("run ID is required for legacy log syntax")?;
                api.get(&run_logs_path(&run_id, after.unwrap_or(0))?)
                    .await?
            }
        },
        Command::Withdraw { request_id } => {
            api.post(
                &format!("/v1/requests/{}/withdraw", segment(&request_id)?),
                json!({}),
            )
            .await?
        }
        Command::Retry { request_id } => {
            api.post(
                &format!("/v1/requests/{}/retry", segment(&request_id)?),
                json!({}),
            )
            .await?
        }
        Command::Priority {
            request_id,
            priority,
            reason,
        } => {
            api.post(
                &format!("/v1/requests/{}/priority", segment(&request_id)?),
                json!({"priority":priority.as_str(),"reason":reason}),
            )
            .await?
        }
        Command::Drain => api.post("/v1/drain", json!({})).await?,
        Command::Resume => api.post("/v1/resume", json!({})).await?,
        Command::ResolveUnknown {
            lane,
            action,
            evidence,
            reason,
        } => {
            let action = match action {
                Resolution::ConfirmStopped => "confirm_stopped",
                Resolution::AdoptRunning => "adopt_running",
            };
            api.post(
                &format!("/v1/lanes/{}/resolve-unknown", segment(&lane)?),
                json!({"action":action,"evidence":evidence,"reason":reason}),
            )
            .await?
        }
        Command::Health => api.get("/ready").await?,
    };
    output(&value)?;
    Ok(0)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_resource_identifiers_cannot_change_route() {
        assert!(segment("../drain").is_err());
        assert!(segment("lane?scope=all").is_err());
        assert!(segment("demo-test").is_ok());
    }
    #[test]
    fn submit_requires_one_unambiguous_environment_selector() {
        let base = [
            "shipbraid",
            "submit",
            "--revision",
            "0123456789012345678901234567890123456789",
            "--components",
            "app",
        ];
        let parse = |extra: &[&str]| Cli::try_parse_from(base.iter().chain(extra.iter()).copied());
        assert!(parse(&["--lane", "dev"]).is_ok());
        assert!(parse(&["--project", "app", "--environment", "dev"]).is_ok());
        assert!(parse(&[]).is_err());
        assert!(parse(&["--project", "app"]).is_err());
        assert!(parse(&["--lane", "dev", "--project", "app", "--environment", "dev"]).is_err());
    }
    #[test]
    fn task_keys_survive_session_changes_and_separate_release_intents() {
        let mut body = json!({"task_id":"issue/42", "revision":"abc", "components":["app"], "session_id":"session-a"});
        let first = task_idempotency_key(&body);
        body["session_id"] = json!("session-b");
        assert_eq!(first, task_idempotency_key(&body));
        body.as_object_mut().unwrap().remove("session_id");
        assert_eq!(first, task_idempotency_key(&body));
        body["revision"] = json!("def");
        assert_ne!(first, task_idempotency_key(&body));
        assert!(first.len() <= 128);
    }
    #[test]
    fn machine_local_errors_do_not_expose_error_chains() {
        let error = anyhow::anyhow!(
            "private-token https://user:password@example.invalid /secret/config.toml"
        );
        let rendered = failure_value(&error).to_string();
        assert!(!rendered.contains("private-token"));
        assert!(!rendered.contains("password"));
        assert!(!rendered.contains("config.toml"));
        assert_eq!(failure_value(&error)["error"]["code"], "local_error");
    }
    #[test]
    fn cli_accepts_agent_context_and_bounded_query_flags() {
        let cli = Cli::try_parse_from([
            "shipbraid",
            "--json",
            "submit",
            "--lane",
            "dev",
            "--revision",
            "abc",
            "--components",
            "app",
            "--task-id",
            "issue/42",
            "--session-id",
            "new-session",
            "--collaborator",
            "agent-b",
        ])
        .unwrap();
        assert!(cli.json);
        assert!(
            Cli::try_parse_from([
                "shipbraid",
                "list",
                "request",
                "--task-id",
                "issue/42",
                "--limit",
                "200"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["shipbraid", "list", "request", "--limit", "201"]).is_err());
        assert!(Cli::try_parse_from(["shipbraid", "system"]).is_ok());
    }
    #[test]
    fn logs_keep_legacy_run_syntax_and_parse_explicit_targets() {
        let legacy = parse_cli(["shipbraid", "logs", "run-123", "--after", "42"]).unwrap();
        let Command::Logs {
            run_id,
            after,
            target,
        } = legacy.command
        else {
            panic!("expected logs command");
        };
        assert_eq!(run_id.as_deref(), Some("run-123"));
        assert_eq!(after, Some(42));
        assert!(target.is_none());

        let legacy_default = parse_cli(["shipbraid", "logs", "run-123"]).unwrap();
        assert!(matches!(
            legacy_default.command,
            Command::Logs {
                run_id: Some(ref run_id),
                after: None,
                target: None,
            } if run_id == "run-123"
        ));

        let explicit_run =
            parse_cli(["shipbraid", "logs", "run", "run-456", "--after", "7"]).unwrap();
        assert!(matches!(
            explicit_run.command,
            Command::Logs {
                run_id: None,
                target: Some(LogTarget::Run { ref run_id, after: 7 }),
                ..
            } if run_id == "run-456"
        ));

        let request = parse_cli([
            "shipbraid",
            "--json",
            "logs",
            "request",
            "request-789",
            "--cursor",
            "opaque.cursor",
        ])
        .unwrap();
        assert!(matches!(
            request.command,
            Command::Logs {
                run_id: None,
                target: Some(LogTarget::Request {
                    ref request_id,
                    cursor: Some(ref cursor),
                }),
                ..
            } if request_id == "request-789" && cursor == "opaque.cursor"
        ));

        assert!(parse_cli(["shipbraid", "logs"]).is_err());
        assert!(parse_cli(["shipbraid", "logs", "--after", "7"]).is_err());
        assert!(matches!(
            parse_cli(["shipbraid", "logs", "run-123", "run", "run-456"]),
            Err(CliParseError::Validation(CliFailure { code, .. }))
                if code == "invalid_arguments"
        ));
        assert!(matches!(
            parse_cli([
                "shipbraid",
                "logs",
                "--after",
                "7",
                "request",
                "request-789"
            ]),
            Err(CliParseError::Validation(CliFailure { code, .. }))
                if code == "invalid_arguments"
        ));
    }
    #[test]
    fn logs_accept_global_json_before_and_after_nested_commands() {
        let cases: [&[&str]; 6] = [
            &["shipbraid", "--json", "logs", "request", "request-1"],
            &["shipbraid", "logs", "--json", "request", "request-1"],
            &["shipbraid", "logs", "request", "--json", "request-1"],
            &["shipbraid", "logs", "request", "request-1", "--json"],
            &["shipbraid", "logs", "--json", "run", "run-1"],
            &["shipbraid", "logs", "run", "run-1", "--json"],
        ];
        for arguments in cases {
            let cli = parse_cli(arguments.iter().copied()).unwrap();
            assert!(cli.json, "failed to parse: {arguments:?}");
            assert!(matches!(
                cli.command,
                Command::Logs {
                    target: Some(_),
                    ..
                }
            ));
        }
    }
    #[test]
    fn request_log_cursor_is_encoded_as_one_query_value() {
        let base = reqwest::Url::parse("https://coordinator.example/prefix/").unwrap();
        let cursor = "opaque+/=& cursor λ";
        let path = request_logs_path(&base, "request-123", Some(cursor)).unwrap();
        let url = base.join(path.trim_start_matches('/')).unwrap();
        let pairs: Vec<_> = url.query_pairs().into_owned().collect();
        assert_eq!(
            pairs,
            vec![
                ("limit".to_owned(), "200".to_owned()),
                ("cursor".to_owned(), cursor.to_owned())
            ]
        );
    }
    #[test]
    fn api_rejects_urls_with_embedded_credentials() {
        assert!(Api::new("https://user:secret@example.com", None).is_err());
        assert!(Api::new("file:///tmp/socket", None).is_err());
        assert!(Api::new("http://192.0.2.1:8787", None).is_err());
        assert!(Api::new("http://127.0.0.1:8787", None).is_ok());
        assert!(Api::new("https://example.com", None).is_ok());
    }
}
