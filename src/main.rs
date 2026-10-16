use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(version, about = "发布请求协调器")]
struct Cli {
    #[arg(
        long,
        global = true,
        env = "AUTO_DELIVER_URL",
        default_value = "http://127.0.0.1:8787"
    )]
    url: String,
    #[arg(
        long,
        global = true,
        env = "AUTO_DELIVER_TOKEN",
        hide_env_values = true
    )]
    token: Option<String>,
    #[arg(
        long,
        global = true,
        env = "AUTO_DELIVER_TOKEN_FILE",
        conflicts_with = "token"
    )]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
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
    },
    Events {
        #[arg(long)]
        lane: String,
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
    Logs {
        run_id: String,
        #[arg(long, default_value_t = 0)]
        after: u64,
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
        let url = self.base.join(path.trim_start_matches('/'))?;
        let mut request = self.client.request(method, url);
        if path.starts_with("/v1/") {
            let token = self
                .token
                .as_deref()
                .context("set AUTO_DELIVER_TOKEN or --token")?;
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        if let Some(key) = key {
            request = request.header("Idempotency-Key", key);
        }
        let response = request.send().await.context("coordinator request failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .await
            .context("coordinator returned non-JSON response")?;
        if !status.is_success() {
            bail!("HTTP {}: {}", status.as_u16(), value);
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
fn output(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "auto_deliver=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    match run(Cli::parse()).await {
        Ok(code) => std::process::ExitCode::from(code),
        Err(error) => {
            eprintln!("{error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
async fn run(cli: Cli) -> Result<u8> {
    if let Command::Serve { config } = &cli.command {
        auto_deliver::service::serve(auto_deliver::config::Config::load(config)?).await?;
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
        Command::Serve { .. } => unreachable!(),
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
        } => {
            let key = idempotency_key.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            eprintln!("idempotency-key: {key}");
            let mut body = json!({"revision":revision,"components":components,"coverage_mode":coverage.as_str(),"priority":priority.as_str(),"deadline_at":deadline,"reason":reason});
            if let Some(lane) = lane {
                body["lane_id"] = lane.into();
            } else {
                body["project"] = project.context("project is required without lane")?.into();
                body["environment"] = environment
                    .context("environment is required without lane")?
                    .into();
            }
            api.call(Method::POST, "/v1/requests", Some(body), Some(&key))
                .await?
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
            loop {
                let value = api.get(&path).await?;
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
                if started.elapsed() >= Duration::from_secs(timeout_seconds) {
                    output(&value)?;
                    eprintln!("local wait timeout; server request remains active");
                    return Ok(124);
                }
                tokio::time::sleep(Duration::from_millis(poll_ms)).await;
            }
        }
        Command::Inspect { kind, id } => {
            api.get(&format!("/v1/{}/{}", kind.path(), segment(&id)?))
                .await?
        }
        Command::List { kind, lane } => {
            let mut path = format!("/v1/{}", kind.path());
            if let Some(lane) = lane {
                path.push_str(&format!("?lane_id={}", segment(&lane)?));
            }
            api.get(&path).await?
        }
        Command::Events { lane, after } => {
            let path = format!(
                "/v1/events?after={after}&limit=200&lane_id={}",
                segment(&lane)?
            );
            api.get(&path).await?
        }
        Command::Logs { run_id, after } => {
            api.get(&format!(
                "/v1/runs/{}/logs?after={after}&limit=200",
                segment(&run_id)?
            ))
            .await?
        }
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
            "auto-deliver",
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
    fn api_rejects_urls_with_embedded_credentials() {
        assert!(Api::new("https://user:secret@example.com", None).is_err());
        assert!(Api::new("file:///tmp/socket", None).is_err());
        assert!(Api::new("http://192.0.2.1:8787", None).is_err());
        assert!(Api::new("http://127.0.0.1:8787", None).is_ok());
        assert!(Api::new("https://example.com", None).is_ok());
    }
}
