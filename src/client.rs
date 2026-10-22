use anyhow::{Context, Result, bail};
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientFailure {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

impl ClientFailure {
    pub fn new(code: &str, message: &str, retryable: bool) -> Self {
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

impl std::fmt::Display for ClientFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ClientFailure {}

pub struct Api {
    pub client: Client,
    pub base: reqwest::Url,
    pub token: Option<String>,
}

impl Api {
    pub fn new(url: &str, token: Option<String>) -> Result<Self> {
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

    pub async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        key: Option<&str>,
    ) -> Result<Value> {
        self.call_with_timeout(method, path, body, key, None).await
    }

    pub async fn call_with_timeout(
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
        let response = request.send().await.map_err(|_| ClientFailure::new(
            "transport_error", "The coordinator response was not received. The operation may have been accepted; recover using the original request or idempotency key.", true,
        ))?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let value: Value = response.json().await.map_err(|_| {
            let mut failure = ClientFailure::new(
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
            let mut failure = ClientFailure::new(
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

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.call(Method::GET, path, None, None).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.call(Method::POST, path, Some(body), None).await
    }
}

pub fn segment(value: &str) -> Result<&str> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("invalid resource identifier");
    }
    Ok(value)
}

pub async fn canonical_task_intent(api: &Api, body: &Value) -> Result<Value> {
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

pub fn task_idempotency_key(body: &Value) -> String {
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

pub fn run_logs_path(run_id: &str, after: u64) -> Result<String> {
    Ok(format!(
        "/v1/runs/{}/logs?after={after}&limit=200",
        segment(run_id)?
    ))
}

pub fn request_logs_path(
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

#[derive(Debug, Clone)]
pub struct SubmitParams {
    pub lane_id: Option<String>,
    pub project: Option<String>,
    pub environment: Option<String>,
    pub revision: String,
    pub components: Vec<String>,
    pub coverage: Option<String>,
    pub priority: Option<String>,
    pub deadline_at: Option<String>,
    pub reason: Option<String>,
    pub idempotency_key: Option<String>,
    pub task_id: Option<String>,
    pub session_id: Option<String>,
    pub collaborators: Vec<String>,
    pub parameters: Option<BTreeMap<String, String>>,
}

pub async fn execute_submit(api: &Api, params: SubmitParams) -> Result<(Value, String)> {
    if params.revision.is_empty() {
        return Err(ClientFailure::new("invalid_arguments", "revision is required.", false).into());
    }
    if params.components.is_empty() {
        return Err(
            ClientFailure::new("invalid_arguments", "components cannot be empty.", false).into(),
        );
    }
    match (&params.lane_id, &params.project, &params.environment) {
        (Some(_), None, None) => {}
        (None, Some(_), Some(_)) => {}
        _ => {
            return Err(ClientFailure::new(
                "invalid_arguments",
                "Specify either lane_id or both project and environment, but not both.",
                false,
            )
            .into());
        }
    }

    let mut components = params.components;
    components.sort();
    components.dedup();

    let mut collaborators = params.collaborators;
    collaborators.sort();
    collaborators.dedup();

    let deadline = params
        .deadline_at
        .map(|value| {
            chrono::DateTime::parse_from_rfc3339(&value).map(|time| {
                time.with_timezone(&chrono::Utc)
                    .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
            })
        })
        .transpose()
        .map_err(|_| {
            ClientFailure::new(
                "invalid_deadline",
                "deadline must be a valid RFC3339 timestamp.",
                false,
            )
        })?;

    let coverage_mode = params.coverage.unwrap_or_else(|| "exact".into());
    if coverage_mode != "exact" && coverage_mode != "descendant" {
        return Err(ClientFailure::new(
            "invalid_arguments",
            "coverage must be 'exact' or 'descendant'.",
            false,
        )
        .into());
    }

    let priority = params.priority.unwrap_or_else(|| "normal".into());
    if priority != "normal" && priority != "urgent" {
        return Err(ClientFailure::new(
            "invalid_arguments",
            "priority must be 'normal' or 'urgent'.",
            false,
        )
        .into());
    }

    let mut body = json!({
        "revision": params.revision,
        "components": components,
        "coverage_mode": coverage_mode,
        "priority": priority,
        "deadline_at": deadline,
        "reason": params.reason.unwrap_or_default(),
    });

    if let Some(parameters) = params.parameters {
        for (k, v) in &parameters {
            if k.is_empty() || v.is_empty() {
                return Err(ClientFailure::new(
                    "invalid_parameter",
                    "Parameter names and values must be nonempty.",
                    false,
                )
                .into());
            }
        }
        if !parameters.is_empty() {
            body["parameters"] = json!(parameters);
        }
    }

    if let Some(task_id) = &params.task_id {
        body["task_id"] = json!(task_id);
    }
    if let Some(session_id) = &params.session_id {
        body["session_id"] = json!(session_id);
    }
    if !collaborators.is_empty() {
        body["collaborators"] = json!(collaborators);
    }
    if let Some(lane) = &params.lane_id {
        body["lane_id"] = json!(lane);
    } else {
        body["project"] = json!(params.project.as_ref().unwrap());
        body["environment"] = json!(params.environment.as_ref().unwrap());
    }

    let key = if let Some(key) = params.idempotency_key {
        key
    } else if params.task_id.is_some() {
        task_idempotency_key(&canonical_task_intent(api, &body).await?)
    } else {
        uuid::Uuid::new_v4().to_string()
    };

    let mut result = match api
        .call(Method::POST, "/v1/requests", Some(body), Some(&key))
        .await
    {
        Ok(value) => value,
        Err(mut error) => {
            if let Some(failure) = error.downcast_mut::<ClientFailure>() {
                failure.idempotency_key = Some(key.clone());
            }
            return Err(error);
        }
    };
    if let Some(object) = result.as_object_mut() {
        object.insert("idempotency_key".into(), json!(key));
    }
    Ok((result, key))
}

#[derive(Debug)]
pub struct WaitOutcome {
    pub value: Value,
    pub is_timeout: bool,
}

pub async fn wait_request_status(
    api: &Api,
    request_id: &str,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<WaitOutcome> {
    if poll_interval.is_zero() {
        return Err(ClientFailure::new(
            "invalid_arguments",
            "poll interval must be positive",
            false,
        )
        .into());
    }
    let path = format!("/v1/requests/{}", segment(request_id)?);
    let started = Instant::now();
    let budget = timeout;
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
                let status = value.get("status").and_then(Value::as_str);
                let is_terminal = matches!(
                    status,
                    Some("satisfied" | "needs_attention" | "expired" | "withdrawn")
                );
                if is_terminal {
                    return Ok(WaitOutcome {
                        value,
                        is_timeout: false,
                    });
                }
                last_observation = Some(value);
            }
            Err(error) => {
                if !error
                    .downcast_ref::<ClientFailure>()
                    .is_some_and(|failure| failure.retryable)
                {
                    return Err(error);
                }
                failures = failures.saturating_add(1);
                let failure = error
                    .downcast_ref::<ClientFailure>()
                    .expect("checked structured failure");
                let backoff = poll_interval
                    .max(Duration::from_millis(100))
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
            let mut value = last_observation.unwrap_or_else(|| {
                json!({
                    "request_id": request_id,
                    "observation_available": false
                })
            });
            value["local_wait_timeout"] = json!(true);
            return Ok(WaitOutcome {
                value,
                is_timeout: true,
            });
        }
        if failures == 0 {
            tokio::time::sleep(poll_interval.min(remaining)).await;
        }
    }
}
