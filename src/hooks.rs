use std::{process::Stdio, time::Duration};

use anyhow::{Context, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};
use uuid::Uuid;

use crate::{
    config::HookConfig,
    domain::{Command as LedgerCommand, DeliveryRun, DeploymentReceipt, Lane},
};

const MAX_HOOK_REQUEST_BYTES: usize = 256 * 1024;
const MAX_HOOK_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_HOOK_LOGS: usize = 100;
const MAX_HOOK_LOG_BYTES: usize = 2048;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookOperation {
    Dispatch,
    Observe,
    Verify,
}

#[derive(Clone, Debug, Serialize)]
pub struct HookEnvelope<'a> {
    pub version: u8,
    pub operation: HookOperation,
    pub command: Option<&'a LedgerCommand>,
    pub run: &'a DeliveryRun,
    pub lane: &'a Lane,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookStatus {
    Started,
    Running,
    Completed,
    Failed,
    Unknown,
    Verified,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HookLog {
    pub sequence: u64,
    pub level: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HookReply {
    pub version: u8,
    pub run_id: Uuid,
    pub generation: u64,
    #[serde(default)]
    pub executor_ref: Option<String>,
    pub status: HookStatus,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub environment_known_unchanged: Option<bool>,
    #[serde(default)]
    pub logs: Vec<HookLog>,
    #[serde(default)]
    pub receipt: Option<DeploymentReceipt>,
}

#[derive(Clone, Debug)]
pub enum HookCall {
    Accepted,
    Reply(Box<HookReply>),
}

#[derive(Clone, Copy, Debug)]
pub struct HookExecutor;

impl HookExecutor {
    pub async fn call(
        config: &HookConfig,
        envelope: &HookEnvelope<'_>,
    ) -> anyhow::Result<HookCall> {
        let call_started_at = Utc::now();
        let input = serde_json::to_vec(envelope).context("serialize hook envelope")?;
        if input.len() > MAX_HOOK_REQUEST_BYTES {
            bail!("hook envelope exceeds configured size limit");
        }
        let call = match config {
            HookConfig::Command {
                program,
                args,
                cwd,
                timeout_ms,
            } => {
                let mut command = Command::new(program);
                command.args(args);
                if let Some(cwd) = cwd {
                    command.current_dir(cwd);
                }
                call_process(command, input, *timeout_ms).await?
            }
            HookConfig::Script {
                path,
                interpreter,
                args,
                cwd,
                timeout_ms,
            } => {
                let mut command = Command::new(interpreter);
                command.arg(path).args(args);
                if let Some(cwd) = cwd {
                    command.current_dir(cwd);
                } else if let Some(parent) = path.parent() {
                    command.current_dir(parent);
                }
                call_process(command, input, *timeout_ms).await?
            }
            HookConfig::Webhook {
                url,
                token_env,
                timeout_ms,
            } => call_webhook(url, token_env, *timeout_ms, input).await?,
        };
        match call {
            HookCall::Accepted => Ok(HookCall::Accepted),
            HookCall::Reply(reply) => {
                validate_reply(&reply, envelope, call_started_at, Utc::now())?;
                Ok(HookCall::Reply(reply))
            }
        }
    }
}

fn validate_reply(
    reply: &HookReply,
    envelope: &HookEnvelope<'_>,
    call_started_at: chrono::DateTime<Utc>,
    received_at: chrono::DateTime<Utc>,
) -> anyhow::Result<()> {
    if reply.version != 1
        || reply.run_id != envelope.run.id
        || reply.generation != envelope.run.generation
    {
        bail!("hook reply does not match the active run generation");
    }
    if reply
        .executor_ref
        .as_ref()
        .is_some_and(|value| value.trim().is_empty() || value.len() > 512)
    {
        bail!("hook reply has an invalid executor reference");
    }
    if matches!(
        reply.status,
        HookStatus::Started | HookStatus::Running | HookStatus::Completed | HookStatus::Verified
    ) && reply.executor_ref.is_none()
    {
        bail!("hook reply omitted its stable executor reference");
    }
    if reply.logs.len() > MAX_HOOK_LOGS
        || reply.logs.iter().any(|log| {
            log.message.len() > MAX_HOOK_LOG_BYTES
                || !matches!(log.level.as_str(), "debug" | "info" | "warn" | "error")
        })
    {
        bail!("hook reply logs exceed configured limits");
    }
    if reply
        .message
        .as_ref()
        .is_some_and(|message| message.len() > MAX_HOOK_LOG_BYTES)
    {
        bail!("hook reply message exceeds configured limits");
    }
    match (envelope.operation, reply.status, reply.receipt.as_ref()) {
        (HookOperation::Verify, HookStatus::Verified, Some(receipt)) => {
            if receipt.run_id != envelope.run.id
                || receipt.generation != envelope.run.generation
                || receipt.source_revision != envelope.run.target.revision
                || receipt.profile_digest != envelope.run.target.profile_digest
                || receipt.observed_at < call_started_at
                || receipt.observed_at > received_at
                || receipt.component_digests.keys().collect::<Vec<_>>()
                    != envelope.run.target.components.iter().collect::<Vec<_>>()
                || receipt.verification_report.trim().is_empty()
            {
                bail!("verification receipt does not match the frozen run target");
            }
        }
        (HookOperation::Verify, HookStatus::Unknown | HookStatus::Failed, None) => {}
        (HookOperation::Verify, _, _) => {
            bail!("verify hook must return verified with a matching receipt");
        }
        (_, _, None) => {}
        _ => bail!("only a verify hook may return a receipt"),
    }
    Ok(())
}

async fn call_process(
    mut command: Command,
    input: Vec<u8>,
    timeout_ms: u64,
) -> anyhow::Result<HookCall> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("start configured hook process")?;
    let stdin = child.stdin.take().context("hook stdin unavailable")?;
    let stdout = child.stdout.take().context("hook stdout unavailable")?;
    let stderr = child.stderr.take().context("hook stderr unavailable")?;
    let output = async {
        let write_stdin = async move {
            let mut stdin = stdin;
            stdin.write_all(&input).await?;
            stdin.shutdown().await
        };
        let (write_result, stdout, stderr, status) = tokio::join!(
            write_stdin,
            read_bounded(stdout),
            read_bounded(stderr),
            child.wait()
        );
        write_result.context("write hook envelope")?;
        let stdout = stdout.context("read hook response")?;
        let _stderr = stderr.context("read hook diagnostic stream")?;
        let status = status.context("wait for hook process")?;
        if !status.success() {
            bail!("configured hook exited with a nonzero status");
        }
        parse_response(&stdout)
    };
    match timeout(Duration::from_millis(timeout_ms), output).await {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            bail!("configured hook timed out");
        }
    }
}

async fn read_bounded<R>(reader: R) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take((MAX_HOOK_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > MAX_HOOK_RESPONSE_BYTES {
        return Err(std::io::Error::other("hook output exceeded size limit"));
    }
    Ok(bytes)
}

fn parse_response(bytes: &[u8]) -> anyhow::Result<HookCall> {
    if bytes.is_empty() {
        bail!("configured hook returned no JSON response");
    }
    serde_json::from_slice(bytes)
        .map(HookCall::Reply)
        .context("configured hook returned invalid JSON")
}

async fn call_webhook(
    url: &str,
    token_env: &str,
    timeout_ms: u64,
    input: Vec<u8>,
) -> anyhow::Result<HookCall> {
    let token = std::env::var(token_env).context("webhook bearer credential is unavailable")?;
    if token.is_empty() || token.len() > 8192 {
        bail!("webhook bearer credential is invalid");
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .context("create webhook client")?;
    let mut response = client
        .post(url)
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(input)
        .send()
        .await
        .context("webhook request failed or timed out")?;
    if response.status() == reqwest::StatusCode::ACCEPTED {
        return Ok(HookCall::Accepted);
    }
    if !response.status().is_success() {
        bail!("webhook returned a non-success status");
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_HOOK_RESPONSE_BYTES as u64)
    {
        bail!("webhook response exceeds configured size limit");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.context("read webhook response")? {
        if bytes.len() + chunk.len() > MAX_HOOK_RESPONSE_BYTES {
            bail!("webhook response exceeds configured size limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        Ok(HookCall::Accepted)
    } else {
        parse_response(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CoverageMode, Priority, Target};
    use chrono::Utc;
    use std::collections::{BTreeMap, BTreeSet};

    fn fixture() -> (Lane, DeliveryRun) {
        let target = Target {
            repository_id: "repo".into(),
            revision: "a".repeat(40),
            components: BTreeSet::from(["app".into()]),
            profile_digest: "profile".into(),
            release_line_id: "line".into(),
            config_version: 1,
            coverage_mode: CoverageMode::Exact,
        };
        let run_id = Uuid::new_v4();
        let lane = Lane {
            id: "dev".into(),
            project: "p".into(),
            environment: "dev".into(),
            enabled: true,
            repository_id: "repo".into(),
            release_ref: "refs/heads/main".into(),
            release_line_id: "line".into(),
            profile_digest: "profile".into(),
            config_version: 1,
            descendant_coverage_enabled: false,
            preemption_enabled: false,
            allowed_revisions: BTreeSet::new(),
            allowed_components: BTreeSet::from(["app".into()]),
            active_run_id: Some(run_id),
            generation: 1,
            blocked_reason: None,
            failure_paused: false,
        };
        let run = DeliveryRun {
            id: run_id,
            lane_id: lane.id.clone(),
            target,
            effective_priority: Priority::Normal,
            state: crate::domain::RunState::Dispatching,
            phase: crate::domain::Phase::Deploying,
            generation: 1,
            executor_ref: None,
            replacement_run_id: None,
            cancel_reason: None,
            receipt_id: None,
            created_at: Utc::now(),
            phase_started_at: Utc::now(),
            updated_at: Utc::now(),
        };
        (lane, run)
    }

    #[test]
    fn reply_is_fenced_to_run_generation_and_operation() {
        let (lane, run) = fixture();
        let envelope = HookEnvelope {
            version: 1,
            operation: HookOperation::Dispatch,
            command: None,
            run: &run,
            lane: &lane,
        };
        let reply = HookReply {
            version: 1,
            run_id: run.id,
            generation: run.generation + 1,
            executor_ref: Some("unit".into()),
            status: HookStatus::Started,
            message: None,
            environment_known_unchanged: None,
            logs: vec![],
            receipt: None,
        };
        assert!(validate_reply(&reply, &envelope, Utc::now(), Utc::now()).is_err());
    }

    #[test]
    fn receipt_must_match_frozen_target_components_and_revision() {
        let (lane, run) = fixture();
        let envelope = HookEnvelope {
            version: 1,
            operation: HookOperation::Verify,
            command: None,
            run: &run,
            lane: &lane,
        };
        let call_started_at = Utc::now();
        let receipt = DeploymentReceipt {
            id: Uuid::new_v4(),
            run_id: run.id,
            generation: run.generation,
            observed_at: Utc::now(),
            source_revision: run.target.revision.clone(),
            component_digests: BTreeMap::new(),
            profile_digest: run.target.profile_digest.clone(),
            verification_report: "ok".into(),
            result: crate::domain::ReceiptResult::Success,
        };
        let reply = HookReply {
            version: 1,
            run_id: run.id,
            generation: run.generation,
            executor_ref: Some("unit".into()),
            status: HookStatus::Verified,
            message: None,
            environment_known_unchanged: None,
            logs: vec![],
            receipt: Some(receipt),
        };
        assert!(validate_reply(&reply, &envelope, call_started_at, Utc::now()).is_err());
    }

    #[test]
    fn accepted_transport_reply_is_distinct_from_started_proof() {
        let value = serde_json::json!({"version":1,"run_id":Uuid::new_v4(),"generation":1,"status":"started","executor_ref":"job"});
        let reply: HookReply = serde_json::from_value(value).unwrap();
        assert_eq!(reply.status, HookStatus::Started);
        assert!(matches!(HookCall::Accepted, HookCall::Accepted));
    }
}
