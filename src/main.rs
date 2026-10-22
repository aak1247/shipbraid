use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use shipbraid::client::{
    Api, ClientFailure as CliFailure, SubmitParams, execute_submit, request_logs_path,
    run_logs_path, segment, wait_request_status,
};
use std::{path::PathBuf, time::Duration};

mod local_cli;

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
    /// Run one project's coordinator in the foreground without an HTTP server.
    Run(local_cli::RunArgs),
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
    /// Clear a failure pause on a lane after operator intervention.
    Unpause {
        /// Lane identifier to unpause.
        #[arg(required_unless_present = "lane")]
        lane_id: Option<String>,
        #[arg(long, conflicts_with = "lane_id")]
        lane: Option<String>,
    },
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
    /// Start the Model Context Protocol (MCP) stdio server.
    Mcp,
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
impl Cli {
    fn validate(&self) -> std::result::Result<(), CliFailure> {
        match &self.command {
            Command::Run(args) => args.validate(),
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

pub(crate) fn output(value: &Value) -> Result<()> {
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
    if let Command::Run(args) = &cli.command {
        return local_cli::run(args, cli.json).await;
    }
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
    if let Command::Mcp = &cli.command {
        return shipbraid::mcp::run_stdio(api).await;
    }
    let value = match cli.command {
        Command::Serve { .. } | Command::Doctor { .. } | Command::Run(_) | Command::Mcp => {
            unreachable!()
        }
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
            collaborators,
            parameters,
        } => {
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
            let (result, key) = execute_submit(
                &api,
                SubmitParams {
                    lane_id: lane,
                    project,
                    environment,
                    revision,
                    components,
                    coverage: Some(coverage.as_str().to_owned()),
                    priority: Some(priority.as_str().to_owned()),
                    deadline_at: deadline,
                    reason: Some(reason),
                    idempotency_key,
                    task_id,
                    session_id,
                    collaborators,
                    parameters: if frozen_parameters.is_empty() {
                        None
                    } else {
                        Some(frozen_parameters)
                    },
                },
            )
            .await?;
            if !cli.json {
                eprintln!("idempotency-key: {key}");
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
            let outcome = wait_request_status(
                &api,
                &request_id,
                Duration::from_secs(timeout_seconds),
                Duration::from_millis(poll_ms),
            )
            .await?;
            output(&outcome.value)?;
            if outcome.is_timeout {
                if !cli.json {
                    eprintln!("local wait timeout; server request remains active");
                }
                return Ok(124);
            }
            let code = match outcome.value.get("status").and_then(Value::as_str) {
                Some("satisfied") => 0,
                Some("needs_attention") => 2,
                Some("expired") => 3,
                Some("withdrawn") => 4,
                _ => 0,
            };
            return Ok(code);
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
        Command::Unpause { lane_id, lane } => {
            let lane = lane_id.or(lane).expect("clap enforces one lane identifier");
            api.post(&format!("/v1/lanes/{}/unpause", segment(&lane)?), json!({}))
                .await?
        }
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
    use shipbraid::client::task_idempotency_key;
    #[test]
    fn cli_resource_identifiers_cannot_change_route() {
        assert!(segment("../drain").is_err());
        assert!(segment("lane?scope=all").is_err());
        assert!(segment("demo-test").is_ok());
    }
    #[test]
    fn mcp_subcommand_parses_with_global_options() {
        let cli = parse_cli([
            "shipbraid",
            "--url",
            "https://shipbraid.example.com",
            "--token",
            "test-token",
            "mcp",
        ])
        .unwrap();
        assert!(matches!(cli.command, Command::Mcp));
        assert_eq!(cli.url, "https://shipbraid.example.com");
        assert_eq!(cli.token.as_deref(), Some("test-token"));
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
    #[test]
    fn unpause_accepts_positional_or_flag_lane_and_token_options() {
        // Positional lane_id
        let cli = Cli::try_parse_from(["shipbraid", "unpause", "prod-lane"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Unpause {
                lane_id: Some(ref id),
                lane: None
            } if id == "prod-lane"
        ));

        // --lane flag
        let cli = Cli::try_parse_from(["shipbraid", "unpause", "--lane", "prod-lane"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Unpause {
                lane_id: None,
                lane: Some(ref id)
            } if id == "prod-lane"
        ));

        // Missing lane is rejected
        assert!(Cli::try_parse_from(["shipbraid", "unpause"]).is_err());

        // Conflicting positional and flag is rejected
        assert!(
            Cli::try_parse_from(["shipbraid", "unpause", "prod-lane", "--lane", "other-lane"])
                .is_err()
        );

        // Global token options work before and after subcommand
        let cli = Cli::try_parse_from(["shipbraid", "--token", "tok-123", "unpause", "prod-lane"])
            .unwrap();
        assert_eq!(cli.token.as_deref(), Some("tok-123"));

        let cli = Cli::try_parse_from(["shipbraid", "unpause", "prod-lane", "--token", "tok-456"])
            .unwrap();
        assert_eq!(cli.token.as_deref(), Some("tok-456"));

        let cli = Cli::try_parse_from([
            "shipbraid",
            "unpause",
            "prod-lane",
            "--token-file",
            "/tmp/token.txt",
        ])
        .unwrap();
        assert_eq!(
            cli.token_file.as_deref(),
            Some(std::path::Path::new("/tmp/token.txt"))
        );
    }
}
