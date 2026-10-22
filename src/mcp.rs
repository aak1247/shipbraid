use crate::client::{
    Api, ClientFailure, SubmitParams, execute_submit, request_logs_path, run_logs_path, segment,
    wait_request_status,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2024-11-05"];
pub const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

pub fn get_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "system",
            description: "Discover the authenticated principal, tenant, and API capabilities. Note that 202 does not mean deployment is finished; release success requires request satisfied and matching receipt. Unknown status does not unlock deployment ownership.",
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "list_lanes",
            description: "List all lanes accessible by the authenticated token.",
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "inspect_lane",
            description: "Inspect details of a specific lane by lane ID.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "lane_id": {
                        "type": "string",
                        "description": "Identifier of the lane to inspect"
                    }
                },
                "required": ["lane_id"]
            }),
        },
        ToolDefinition {
            name: "get_release_guide",
            description: "Retrieve release guide for a lane (lane_id) or aggregated across lanes for a project (project). Exactly one of lane_id or project must be specified.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "lane_id": {
                        "type": "string",
                        "description": "Lane identifier to fetch lane-specific release guide"
                    },
                    "project": {
                        "type": "string",
                        "description": "Project identifier to fetch aggregated release guide"
                    }
                }
            }),
        },
        ToolDefinition {
            name: "get_request",
            description: "Get release request details by request ID.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "request_id": {
                        "type": "string",
                        "description": "Identifier of the request to inspect"
                    }
                },
                "required": ["request_id"]
            }),
        },
        ToolDefinition {
            name: "list_requests",
            description: "List release requests with optional filters (task_id, principal, status, lane_id, cursor, limit).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "lane_id": { "type": "string", "description": "Filter by lane ID" },
                    "task_id": { "type": "string", "description": "Filter by task ID" },
                    "principal": { "type": "string", "description": "Filter by principal" },
                    "status": { "type": "string", "description": "Filter by request status" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Max items to return (1-200)" },
                    "cursor": { "type": "string", "description": "Pagination cursor" }
                }
            }),
        },
        ToolDefinition {
            name: "get_run",
            description: "Get details of an execution run by run ID.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "run_id": {
                        "type": "string",
                        "description": "Identifier of the execution run"
                    }
                },
                "required": ["run_id"]
            }),
        },
        ToolDefinition {
            name: "get_events",
            description: "Get audit events for a lane. Note that 202 does not mean deployment is finished; release success requires request satisfied and matching receipt.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "lane_id": {
                        "type": "string",
                        "description": "Lane identifier to query audit events"
                    },
                    "after": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Sequence cursor to query events after (default 0)"
                    }
                },
                "required": ["lane_id"]
            }),
        },
        ToolDefinition {
            name: "get_logs",
            description: "Query execution logs by run ID (run mode) or by request ID (request mode). Exactly one of run_id or request_id must be provided.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "run_id": {
                        "type": "string",
                        "description": "Query logs for a fixed run ID"
                    },
                    "after": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Numeric sequence cursor for run logs (default 0)"
                    },
                    "request_id": {
                        "type": "string",
                        "description": "Query logs for the current run of a request ID"
                    },
                    "cursor": {
                        "type": "string",
                        "description": "Opaque cursor for request logs"
                    }
                }
            }),
        },
        ToolDefinition {
            name: "submit_request",
            description: "Submit an immutable release request. 202 does not mean deployment is finished; release success requires request satisfied and matching receipt. Retries must reuse the same idempotency key or task_id. Unknown status does not unlock deployment ownership.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "lane_id": { "type": "string", "description": "Target lane ID (mutually exclusive with project+environment)" },
                    "project": { "type": "string", "description": "Target project (requires environment; mutually exclusive with lane_id)" },
                    "environment": { "type": "string", "description": "Target environment (requires project; mutually exclusive with lane_id)" },
                    "revision": { "type": "string", "description": "Immutable git commit revision SHA" },
                    "components": { "type": "array", "items": { "type": "string" }, "description": "Components to deploy (sorted and deduplicated)" },
                    "coverage": { "type": "string", "enum": ["exact", "descendant"], "description": "Coverage mode (default: exact)" },
                    "priority": { "type": "string", "enum": ["normal", "urgent"], "description": "Request priority (default: normal)" },
                    "deadline_at": { "type": "string", "description": "RFC3339 timestamp with timezone; converted to UTC" },
                    "reason": { "type": "string", "description": "Reason for release request (default: empty)" },
                    "idempotency_key": { "type": "string", "description": "Explicit idempotency key. If omitted, derived from task_id or generated randomly" },
                    "task_id": { "type": "string", "description": "Stable task ID for deterministic idempotency key derivation" },
                    "session_id": { "type": "string", "description": "Creation session ID trace" },
                    "collaborators": { "type": "array", "items": { "type": "string" }, "description": "Principals allowed to manage this request" },
                    "parameters": { "type": "object", "description": "Frozen non-secret execution parameters (key-value map)" }
                },
                "required": ["revision", "components"]
            }),
        },
        ToolDefinition {
            name: "wait_request",
            description: "Wait locally for a release request to reach a terminal state (satisfied, needs_attention, expired, withdrawn) or timeout. 202 does not mean deployment is finished; release success requires request satisfied and matching receipt. Local timeout does not withdraw the server request. Unknown status does not unlock deployment ownership.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "request_id": { "type": "string", "description": "Identifier of the request to wait on" },
                    "timeout_seconds": { "type": "integer", "minimum": 0, "maximum": 600, "description": "Wait timeout in seconds (default: 300, max: 600)" },
                    "poll_seconds": { "type": "number", "minimum": 0.1, "description": "Polling interval in seconds (default: 0.5)" }
                },
                "required": ["request_id"]
            }),
        },
        ToolDefinition {
            name: "withdraw_request",
            description: "Withdraw a pending or waiting release request. Does not cancel other requests sharing an active run.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "request_id": { "type": "string", "description": "Identifier of the request to withdraw" }
                },
                "required": ["request_id"]
            }),
        },
        ToolDefinition {
            name: "retry_request",
            description: "Retry a failed or needs_attention request using original target and deadline. Unknown status does not unlock deployment ownership.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "request_id": { "type": "string", "description": "Identifier of the request to retry" }
                },
                "required": ["request_id"]
            }),
        },
        ToolDefinition {
            name: "raise_priority",
            description: "Raise priority of a release request (e.g. to urgent). Requires urgent or admin capability. Urgent does not preempt hooks v1 execution.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "request_id": { "type": "string", "description": "Identifier of the request to update" },
                    "priority": { "type": "string", "enum": ["normal", "urgent"], "description": "Target priority" },
                    "reason": { "type": "string", "description": "Reason for priority change (default: 'MCP priority update')" }
                },
                "required": ["request_id", "priority"]
            }),
        },
    ]
}

pub struct ToolCallOutcome {
    pub text: String,
    pub is_error: bool,
}

impl ToolCallOutcome {
    pub fn success(value: &Value) -> Self {
        Self {
            text: serde_json::to_string(value).unwrap_or_else(|_| "{}".into()),
            is_error: false,
        }
    }

    pub fn error(failure: &ClientFailure) -> Self {
        Self {
            text: serde_json::to_string(failure).unwrap_or_else(|_| "{}".into()),
            is_error: true,
        }
    }

    pub fn from_anyhow(err: anyhow::Error) -> Self {
        if let Some(failure) = err.downcast_ref::<ClientFailure>() {
            Self::error(failure)
        } else {
            Self::error(&ClientFailure::new("local_error", &err.to_string(), false))
        }
    }
}

pub async fn dispatch_tool_call(api: &Api, name: &str, arguments: &Value) -> ToolCallOutcome {
    match name {
        "system" => match api.get("/v1/system").await {
            Ok(val) => ToolCallOutcome::success(&val),
            Err(e) => ToolCallOutcome::from_anyhow(e),
        },
        "list_lanes" => match api.get("/v1/lanes").await {
            Ok(val) => ToolCallOutcome::success(&val),
            Err(e) => ToolCallOutcome::from_anyhow(e),
        },
        "inspect_lane" => {
            let Some(lane_id) = arguments.get("lane_id").and_then(Value::as_str) else {
                return ToolCallOutcome::error(&ClientFailure::new(
                    "invalid_arguments",
                    "lane_id is required",
                    false,
                ));
            };
            if let Err(e) = segment(lane_id) {
                return ToolCallOutcome::from_anyhow(e);
            }
            match api.get(&format!("/v1/lanes/{lane_id}")).await {
                Ok(val) => ToolCallOutcome::success(&val),
                Err(e) => ToolCallOutcome::from_anyhow(e),
            }
        }
        "get_release_guide" => {
            let lane_id = arguments.get("lane_id").and_then(Value::as_str);
            let project = arguments.get("project").and_then(Value::as_str);
            match (lane_id, project) {
                (Some(lid), None) => {
                    if let Err(e) = segment(lid) {
                        return ToolCallOutcome::from_anyhow(e);
                    }
                    match api.get(&format!("/v1/lanes/{lid}/guide")).await {
                        Ok(val) => ToolCallOutcome::success(&val),
                        Err(e) => ToolCallOutcome::from_anyhow(e),
                    }
                }
                (None, Some(p)) => {
                    if let Err(e) = segment(p) {
                        return ToolCallOutcome::from_anyhow(e);
                    }
                    match api.get(&format!("/v1/guides/{p}")).await {
                        Ok(val) => ToolCallOutcome::success(&val),
                        Err(e) => ToolCallOutcome::from_anyhow(e),
                    }
                }
                _ => ToolCallOutcome::error(&ClientFailure::new(
                    "invalid_arguments",
                    "Specify either lane_id or project, but not both",
                    false,
                )),
            }
        }
        "get_request" => {
            let Some(request_id) = arguments.get("request_id").and_then(Value::as_str) else {
                return ToolCallOutcome::error(&ClientFailure::new(
                    "invalid_arguments",
                    "request_id is required",
                    false,
                ));
            };
            if let Err(e) = segment(request_id) {
                return ToolCallOutcome::from_anyhow(e);
            }
            match api.get(&format!("/v1/requests/{request_id}")).await {
                Ok(val) => ToolCallOutcome::success(&val),
                Err(e) => ToolCallOutcome::from_anyhow(e),
            }
        }
        "list_requests" => {
            let mut url = match api.base.join("v1/requests") {
                Ok(u) => u,
                Err(e) => return ToolCallOutcome::from_anyhow(e.into()),
            };
            {
                let mut query = url.query_pairs_mut();
                if let Some(lane) = arguments.get("lane_id").and_then(Value::as_str) {
                    if let Err(e) = segment(lane) {
                        return ToolCallOutcome::from_anyhow(e);
                    }
                    query.append_pair("lane_id", lane);
                }
                if let Some(task_id) = arguments.get("task_id").and_then(Value::as_str) {
                    query.append_pair("task_id", task_id);
                }
                if let Some(principal) = arguments.get("principal").and_then(Value::as_str) {
                    query.append_pair("principal", principal);
                }
                if let Some(status) = arguments.get("status").and_then(Value::as_str) {
                    query.append_pair("status", status);
                }
                if let Some(limit) = arguments.get("limit") {
                    let Some(l) = limit.as_u64() else {
                        return ToolCallOutcome::error(&ClientFailure::new(
                            "invalid_arguments",
                            "limit must be an integer",
                            false,
                        ));
                    };
                    if !(1..=200).contains(&l) {
                        return ToolCallOutcome::error(&ClientFailure::new(
                            "invalid_arguments",
                            "limit must be between 1 and 200",
                            false,
                        ));
                    }
                    query.append_pair("limit", &l.to_string());
                }
                if let Some(cursor) = arguments.get("cursor").and_then(Value::as_str) {
                    query.append_pair("cursor", cursor);
                }
            }
            let path = format!(
                "/v1/requests{}",
                url.query().map(|q| format!("?{q}")).unwrap_or_default()
            );
            match api.get(&path).await {
                Ok(val) => ToolCallOutcome::success(&val),
                Err(e) => ToolCallOutcome::from_anyhow(e),
            }
        }
        "get_run" => {
            let Some(run_id) = arguments.get("run_id").and_then(Value::as_str) else {
                return ToolCallOutcome::error(&ClientFailure::new(
                    "invalid_arguments",
                    "run_id is required",
                    false,
                ));
            };
            if let Err(e) = segment(run_id) {
                return ToolCallOutcome::from_anyhow(e);
            }
            match api.get(&format!("/v1/runs/{run_id}")).await {
                Ok(val) => ToolCallOutcome::success(&val),
                Err(e) => ToolCallOutcome::from_anyhow(e),
            }
        }
        "get_events" => {
            let Some(lane_id) = arguments.get("lane_id").and_then(Value::as_str) else {
                return ToolCallOutcome::error(&ClientFailure::new(
                    "invalid_arguments",
                    "lane_id is required",
                    false,
                ));
            };
            if let Err(e) = segment(lane_id) {
                return ToolCallOutcome::from_anyhow(e);
            }
            let after = arguments.get("after").and_then(Value::as_u64).unwrap_or(0);
            let path = format!("/v1/events?after={after}&limit=200&lane_id={lane_id}");
            match api.get(&path).await {
                Ok(val) => ToolCallOutcome::success(&val),
                Err(e) => ToolCallOutcome::from_anyhow(e),
            }
        }
        "get_logs" => {
            let run_id = arguments.get("run_id").and_then(Value::as_str);
            let request_id = arguments.get("request_id").and_then(Value::as_str);
            match (run_id, request_id) {
                (Some(rid), None) => {
                    let after = arguments.get("after").and_then(Value::as_u64).unwrap_or(0);
                    let path = match run_logs_path(rid, after) {
                        Ok(p) => p,
                        Err(e) => return ToolCallOutcome::from_anyhow(e),
                    };
                    match api.get(&path).await {
                        Ok(val) => ToolCallOutcome::success(&val),
                        Err(e) => ToolCallOutcome::from_anyhow(e),
                    }
                }
                (None, Some(req_id)) => {
                    let cursor = arguments.get("cursor").and_then(Value::as_str);
                    let path = match request_logs_path(&api.base, req_id, cursor) {
                        Ok(p) => p,
                        Err(e) => return ToolCallOutcome::from_anyhow(e),
                    };
                    match api.get(&path).await {
                        Ok(val) => ToolCallOutcome::success(&val),
                        Err(e) => ToolCallOutcome::from_anyhow(e),
                    }
                }
                _ => ToolCallOutcome::error(&ClientFailure::new(
                    "invalid_arguments",
                    "Specify either run_id or request_id, but not both",
                    false,
                )),
            }
        }
        "submit_request" => {
            let revision = match arguments.get("revision").and_then(Value::as_str) {
                Some(r) => r.to_owned(),
                None => {
                    return ToolCallOutcome::error(&ClientFailure::new(
                        "invalid_arguments",
                        "revision is required",
                        false,
                    ));
                }
            };
            let components = match arguments.get("components").and_then(Value::as_array) {
                Some(arr) => {
                    let mut comps = Vec::new();
                    for item in arr {
                        if let Some(s) = item.as_str() {
                            comps.push(s.to_owned());
                        } else {
                            return ToolCallOutcome::error(&ClientFailure::new(
                                "invalid_arguments",
                                "components elements must be strings",
                                false,
                            ));
                        }
                    }
                    comps
                }
                None => {
                    return ToolCallOutcome::error(&ClientFailure::new(
                        "invalid_arguments",
                        "components is required",
                        false,
                    ));
                }
            };
            let collaborators = match arguments.get("collaborators").and_then(Value::as_array) {
                Some(arr) => {
                    let mut collabs = Vec::new();
                    for item in arr {
                        if let Some(s) = item.as_str() {
                            collabs.push(s.to_owned());
                        } else {
                            return ToolCallOutcome::error(&ClientFailure::new(
                                "invalid_arguments",
                                "collaborators elements must be strings",
                                false,
                            ));
                        }
                    }
                    collabs
                }
                None => Vec::new(),
            };
            let parameters = match arguments.get("parameters") {
                Some(Value::Object(map)) => {
                    let mut bmap = BTreeMap::new();
                    for (k, v) in map {
                        if let Some(s) = v.as_str() {
                            bmap.insert(k.clone(), s.to_owned());
                        } else {
                            return ToolCallOutcome::error(&ClientFailure::new(
                                "invalid_parameter",
                                "parameter values must be strings",
                                false,
                            ));
                        }
                    }
                    Some(bmap)
                }
                Some(_) => {
                    return ToolCallOutcome::error(&ClientFailure::new(
                        "invalid_parameter",
                        "parameters must be a key-value object",
                        false,
                    ));
                }
                None => None,
            };

            let deadline_at = arguments
                .get("deadline_at")
                .or_else(|| arguments.get("deadline"))
                .and_then(Value::as_str)
                .map(|s| s.to_owned());

            let params = SubmitParams {
                lane_id: arguments
                    .get("lane_id")
                    .and_then(Value::as_str)
                    .map(|s| s.to_owned()),
                project: arguments
                    .get("project")
                    .and_then(Value::as_str)
                    .map(|s| s.to_owned()),
                environment: arguments
                    .get("environment")
                    .and_then(Value::as_str)
                    .map(|s| s.to_owned()),
                revision,
                components,
                coverage: arguments
                    .get("coverage")
                    .and_then(Value::as_str)
                    .map(|s| s.to_owned()),
                priority: arguments
                    .get("priority")
                    .and_then(Value::as_str)
                    .map(|s| s.to_owned()),
                deadline_at,
                reason: arguments
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(|s| s.to_owned()),
                idempotency_key: arguments
                    .get("idempotency_key")
                    .and_then(Value::as_str)
                    .map(|s| s.to_owned()),
                task_id: arguments
                    .get("task_id")
                    .and_then(Value::as_str)
                    .map(|s| s.to_owned()),
                session_id: arguments
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(|s| s.to_owned()),
                collaborators,
                parameters,
            };

            match execute_submit(api, params).await {
                Ok((val, _key)) => ToolCallOutcome::success(&val),
                Err(e) => ToolCallOutcome::from_anyhow(e),
            }
        }
        "wait_request" => {
            let Some(request_id) = arguments.get("request_id").and_then(Value::as_str) else {
                return ToolCallOutcome::error(&ClientFailure::new(
                    "invalid_arguments",
                    "request_id is required",
                    false,
                ));
            };
            let timeout_seconds = match arguments.get("timeout_seconds") {
                Some(val) => {
                    let Some(s) = val.as_u64() else {
                        return ToolCallOutcome::error(&ClientFailure::new(
                            "invalid_arguments",
                            "timeout_seconds must be an integer",
                            false,
                        ));
                    };
                    if s > 600 {
                        return ToolCallOutcome::error(&ClientFailure::new(
                            "invalid_arguments",
                            "timeout_seconds exceeds maximum of 600 seconds",
                            false,
                        ));
                    }
                    s
                }
                None => 300,
            };
            let poll_seconds = match arguments.get("poll_seconds") {
                Some(val) => {
                    let Some(s) = val.as_f64() else {
                        return ToolCallOutcome::error(&ClientFailure::new(
                            "invalid_arguments",
                            "poll_seconds must be a number",
                            false,
                        ));
                    };
                    if s <= 0.0 {
                        return ToolCallOutcome::error(&ClientFailure::new(
                            "invalid_arguments",
                            "poll_seconds must be positive",
                            false,
                        ));
                    }
                    s
                }
                None => 0.5,
            };

            let timeout = Duration::from_secs(timeout_seconds);
            let poll_interval = Duration::from_secs_f64(poll_seconds);

            match wait_request_status(api, request_id, timeout, poll_interval).await {
                Ok(outcome) => ToolCallOutcome::success(&outcome.value),
                Err(e) => ToolCallOutcome::from_anyhow(e),
            }
        }
        "withdraw_request" => {
            let Some(request_id) = arguments.get("request_id").and_then(Value::as_str) else {
                return ToolCallOutcome::error(&ClientFailure::new(
                    "invalid_arguments",
                    "request_id is required",
                    false,
                ));
            };
            if let Err(e) = segment(request_id) {
                return ToolCallOutcome::from_anyhow(e);
            }
            match api
                .post(&format!("/v1/requests/{request_id}/withdraw"), json!({}))
                .await
            {
                Ok(val) => ToolCallOutcome::success(&val),
                Err(e) => ToolCallOutcome::from_anyhow(e),
            }
        }
        "retry_request" => {
            let Some(request_id) = arguments.get("request_id").and_then(Value::as_str) else {
                return ToolCallOutcome::error(&ClientFailure::new(
                    "invalid_arguments",
                    "request_id is required",
                    false,
                ));
            };
            if let Err(e) = segment(request_id) {
                return ToolCallOutcome::from_anyhow(e);
            }
            match api
                .post(&format!("/v1/requests/{request_id}/retry"), json!({}))
                .await
            {
                Ok(val) => ToolCallOutcome::success(&val),
                Err(e) => ToolCallOutcome::from_anyhow(e),
            }
        }
        "raise_priority" => {
            let Some(request_id) = arguments.get("request_id").and_then(Value::as_str) else {
                return ToolCallOutcome::error(&ClientFailure::new(
                    "invalid_arguments",
                    "request_id is required",
                    false,
                ));
            };
            if let Err(e) = segment(request_id) {
                return ToolCallOutcome::from_anyhow(e);
            }
            let Some(priority) = arguments.get("priority").and_then(Value::as_str) else {
                return ToolCallOutcome::error(&ClientFailure::new(
                    "invalid_arguments",
                    "priority is required",
                    false,
                ));
            };
            if priority != "normal" && priority != "urgent" {
                return ToolCallOutcome::error(&ClientFailure::new(
                    "invalid_arguments",
                    "priority must be 'normal' or 'urgent'",
                    false,
                ));
            }
            let reason = arguments
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("MCP priority update");
            match api
                .post(
                    &format!("/v1/requests/{request_id}/priority"),
                    json!({ "priority": priority, "reason": reason }),
                )
                .await
            {
                Ok(val) => ToolCallOutcome::success(&val),
                Err(e) => ToolCallOutcome::from_anyhow(e),
            }
        }
        _ => ToolCallOutcome::error(&ClientFailure::new(
            "unknown_tool",
            &format!("Unknown tool: {name}"),
            false,
        )),
    }
}

pub async fn handle_message(api: &Api, raw_json: &str) -> Option<Value> {
    let parsed: Value = match serde_json::from_str(raw_json) {
        Ok(v) => v,
        Err(_) => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {
                    "code": -32700,
                    "message": "Parse error"
                }
            }));
        }
    };

    let Some(obj) = parsed.as_object() else {
        return Some(json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "error": {
                "code": -32600,
                "message": "Invalid Request: expected JSON object"
            }
        }));
    };

    let id = obj.get("id");
    let method = obj.get("method").and_then(Value::as_str);

    // Notifications (no id or id is null): silently ignore, do not send response.
    if id.is_none() || id == Some(&Value::Null) {
        return None;
    }

    let id = id.unwrap().clone();

    let Some(method) = method else {
        return Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32600,
                "message": "Invalid Request: missing method"
            }
        }));
    };

    let params = obj.get("params");

    match method {
        "initialize" => {
            let client_version = params
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str);
            let negotiated_version = match client_version {
                Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => v,
                _ => DEFAULT_PROTOCOL_VERSION,
            };

            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": negotiated_version,
                    "serverInfo": {
                        "name": "shipbraid-mcp",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": {
                        "tools": {}
                    }
                }
            }))
        }
        "tools/list" => {
            let tools = get_tool_definitions()
                .into_iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema
                    })
                })
                .collect::<Vec<_>>();

            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": tools
                }
            }))
        }
        "tools/call" => {
            let Some(params_obj) = params.and_then(Value::as_object) else {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": serde_json::to_string(&ClientFailure::new(
                                "invalid_arguments",
                                "params must be an object",
                                false
                            )).unwrap()
                        }],
                        "isError": true
                    }
                }));
            };

            let tool_name = match params_obj.get("name").and_then(Value::as_str) {
                Some(name) => name,
                None => {
                    return Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": serde_json::to_string(&ClientFailure::new(
                                    "invalid_arguments",
                                    "Tool name is required",
                                    false
                                )).unwrap()
                            }],
                            "isError": true
                        }
                    }));
                }
            };

            let default_args = Value::Object(serde_json::Map::new());
            let arguments = params_obj.get("arguments").unwrap_or(&default_args);

            let outcome = dispatch_tool_call(api, tool_name, arguments).await;

            let mut result_obj = serde_json::Map::new();
            result_obj.insert(
                "content".into(),
                json!([
                    {
                        "type": "text",
                        "text": outcome.text
                    }
                ]),
            );
            if outcome.is_error {
                result_obj.insert("isError".into(), json!(true));
            }

            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": Value::Object(result_obj)
            }))
        }
        _ => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": "Method not found"
            }
        })),
    }
}

pub async fn run_stdio(api: Api) -> Result<u8> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();

    while reader.read_line(&mut line).await? != 0 {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }
        if let Some(response) = handle_message(&api, trimmed).await {
            let mut bytes = serde_json::to_vec(&response)?;
            bytes.push(b'\n');
            stdout.write_all(&bytes).await?;
            stdout.flush().await?;
        }
        line.clear();
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_api() -> Api {
        Api::new("http://127.0.0.1:8787", Some("test-token".into())).unwrap()
    }

    #[tokio::test]
    async fn test_initialize_handshake_protocol_negotiation() {
        let api = dummy_api();

        // 1. Client requests 2025-06-18 -> echoes 2025-06-18
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "clientInfo": {"name": "test-client", "version": "1.0.0"}
            }
        });
        let resp = handle_message(&api, &req.to_string()).await.unwrap();
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(resp["result"]["serverInfo"]["name"], "shipbraid-mcp");
        assert_eq!(
            resp["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert!(resp["result"]["capabilities"]["tools"].is_object());

        // 2. Client requests 2024-11-05 -> echoes 2024-11-05
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05"
            }
        });
        let resp = handle_message(&api, &req.to_string()).await.unwrap();
        assert_eq!(resp["id"], 2);
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");

        // 3. Client requests unknown protocol version -> falls back to 2025-06-18
        let req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "initialize",
            "params": {
                "protocolVersion": "2099-01-01"
            }
        });
        let resp = handle_message(&api, &req.to_string()).await.unwrap();
        assert_eq!(resp["id"], 3);
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");

        // 4. Client omits protocolVersion -> defaults to 2025-06-18
        let req = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "initialize"
        });
        let resp = handle_message(&api, &req.to_string()).await.unwrap();
        assert_eq!(resp["id"], 4);
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");
    }

    #[tokio::test]
    async fn test_notifications_silently_ignored() {
        let api = dummy_api();

        // Standard initialized notification (no id)
        let note1 = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        assert!(handle_message(&api, &note1.to_string()).await.is_none());

        // Any notification with null id
        let note2 = json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "method": "notifications/cancelled",
            "params": {"requestId": 1}
        });
        assert!(handle_message(&api, &note2.to_string()).await.is_none());

        // Custom notification without id
        let note3 = json!({
            "jsonrpc": "2.0",
            "method": "custom/notify"
        });
        assert!(handle_message(&api, &note3.to_string()).await.is_none());
    }

    #[tokio::test]
    async fn test_jsonrpc_protocol_errors() {
        let api = dummy_api();

        // 1. Parse error (-32700) on malformed JSON
        let resp = handle_message(&api, "{not valid json").await.unwrap();
        assert_eq!(resp["jsonrpc"], "2.0");
        assert!(resp["id"].is_null());
        assert_eq!(resp["error"]["code"], -32700);

        // 2. Invalid Request (-32600) on array batch
        let resp = handle_message(&api, "[1, 2, 3]").await.unwrap();
        assert_eq!(resp["error"]["code"], -32600);

        // 3. Invalid Request (-32600) on primitive string
        let resp = handle_message(&api, "\"just a string\"").await.unwrap();
        assert_eq!(resp["error"]["code"], -32600);

        // 4. Invalid Request (-32600) when missing method
        let resp = handle_message(&api, r#"{"jsonrpc":"2.0","id":10}"#)
            .await
            .unwrap();
        assert_eq!(resp["id"], 10);
        assert_eq!(resp["error"]["code"], -32600);

        // 5. Method not found (-32601) for unknown methods
        let resp = handle_message(
            &api,
            r#"{"jsonrpc":"2.0","id":11,"method":"unknown_method"}"#,
        )
        .await
        .unwrap();
        assert_eq!(resp["id"], 11);
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn test_tools_list_snapshot_and_no_admin_tools() {
        let api = dummy_api();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list"
        });
        let resp = handle_message(&api, &req.to_string()).await.unwrap();
        let tools = resp["result"]["tools"].as_array().expect("tools array");

        // Assert exactly 14 tools
        assert_eq!(
            tools.len(),
            14,
            "MCP must expose exactly 14 tools, found {}",
            tools.len()
        );

        let mut names: Vec<String> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect();
        names.sort();

        let expected_names = vec![
            "get_events",
            "get_logs",
            "get_release_guide",
            "get_request",
            "get_run",
            "inspect_lane",
            "list_lanes",
            "list_requests",
            "raise_priority",
            "retry_request",
            "submit_request",
            "system",
            "wait_request",
            "withdraw_request",
        ];
        assert_eq!(names, expected_names);

        // Assert that NO administrative tools exist in the tool surface
        let prohibited_admin_keywords = [
            "drain",
            "resume",
            "unpause",
            "resolve_unknown",
            "resolve-unknown",
            "resources",
            "admin",
            "delete",
            "clean",
            "purge",
        ];

        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            for keyword in &prohibited_admin_keywords {
                assert!(
                    !name.contains(keyword),
                    "Tool name '{name}' contains prohibited administrative keyword '{keyword}'"
                );
            }
            // Verify description and inputSchema
            assert!(
                !tool["description"].as_str().unwrap().is_empty(),
                "Tool '{name}' must have a description"
            );
            assert_eq!(
                tool["inputSchema"]["type"], "object",
                "Tool '{name}' inputSchema must be type: object"
            );
        }
    }

    #[tokio::test]
    async fn test_unknown_tool_call_returns_structured_is_error() {
        let api = dummy_api();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/call",
            "params": {
                "name": "nonexistent_tool_name",
                "arguments": {}
            }
        });
        let resp = handle_message(&api, &req.to_string()).await.unwrap();
        assert_eq!(resp["id"], 99);
        assert_eq!(resp["result"]["isError"], true);
        let content = resp["result"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");

        let err_json: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(err_json["code"], "unknown_tool");
        assert_eq!(err_json["retryable"], false);
    }

    struct TestHarness {
        _temp: tempfile::TempDir,
        api: Api,
        revision: String,
    }

    fn git_cmd(dir: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git command failed: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    async fn setup_test_harness() -> TestHarness {
        use crate::config::{Config, GuideConfig, LaneConfig, LaneExecutorConfig, TokenConfig};
        use crate::service::Service;
        use std::process::{Command, Stdio};
        use uuid::Uuid;

        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        git_cmd(&work, &["init", "-b", "release"]);
        git_cmd(&work, &["config", "user.name", "MCP Test"]);
        git_cmd(&work, &["config", "user.email", "mcp@example.invalid"]);
        std::fs::write(work.join("app.txt"), "mcp fixture content").unwrap();
        git_cmd(&work, &["add", "app.txt"]);
        git_cmd(&work, &["commit", "-m", "fixture commit"]);
        let revision = git_cmd(&work, &["rev-parse", "HEAD"]);

        let mirror = temp.path().join("mirror.git");
        assert!(
            Command::new("git")
                .args(["clone", "--mirror"])
                .arg(&work)
                .arg(&mirror)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );

        let guides_dir = temp.path().join("guides");
        std::fs::create_dir(&guides_dir).unwrap();
        let overview_file = guides_dir.join("overview.md");
        std::fs::write(&overview_file, "# Project Overview\nGuide for all.").unwrap();
        let dev_file = guides_dir.join("dev.md");
        std::fs::write(&dev_file, "# Dev Guide\nGuide for dev environment.").unwrap();
        let app_file = guides_dir.join("app.md");
        std::fs::write(&app_file, "# App Guide\nGuide for app component.").unwrap();

        let token_val = format!("mcp-token-{}", Uuid::new_v4());
        let env_name = format!("SHIPBRAID_MCP_TEST_{}", Uuid::new_v4().simple());
        unsafe {
            std::env::set_var(&env_name, &token_val);
        }

        let mut comp_map = BTreeMap::new();
        comp_map.insert("app".into(), app_file);

        let lanes = vec![
            LaneConfig {
                id: "lane-one".into(),
                project: "app".into(),
                environment: "dev".into(),
                repository_id: "repo".into(),
                repository_path: mirror.clone(),
                release_ref: "refs/heads/release".into(),
                profile_digest: "profile1".into(),
                components: vec!["app".into()],
                allow_descendant: true,
                allow_preemption: true,
                failure_observe: None,
                resource_admission: None,
                request_parameters: Default::default(),
                enabled: true,
                executor: LaneExecutorConfig::Scripted,
                guide: Some(GuideConfig {
                    project: Some(overview_file),
                    environment: Some(dev_file),
                    components: comp_map,
                }),
                credentials: None,
                hook_environment: None,
            },
            LaneConfig {
                id: "lane-two".into(),
                project: "app".into(),
                environment: "prod".into(),
                repository_id: "repo".into(),
                repository_path: mirror.clone(),
                release_ref: "refs/heads/release".into(),
                profile_digest: "profile2".into(),
                components: vec!["app".into()],
                allow_descendant: true,
                allow_preemption: true,
                failure_observe: None,
                resource_admission: None,
                request_parameters: Default::default(),
                enabled: true,
                executor: LaneExecutorConfig::Scripted,
                guide: None,
                credentials: None,
                hook_environment: None,
            },
        ];

        let tokens = vec![TokenConfig {
            id: "agent-a".into(),
            token_env: env_name.clone(),
            lanes: vec!["lane-one".into(), "lane-two".into()],
            projects: vec![],
            urgent: true,
            admin: false,
            executor: false,
        }];

        let service = Service::open(Config {
            database_path: temp.path().join("ledger.db"),
            tokens,
            lanes,
            ..Config::default()
        })
        .await
        .unwrap();

        unsafe {
            std::env::remove_var(&env_name);
        }

        let app = crate::api::router(service);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://127.0.0.1:{port}");
        let api = Api::new(&url, Some(token_val)).unwrap();

        TestHarness {
            _temp: temp,
            api,
            revision,
        }
    }

    #[tokio::test]
    async fn test_tools_call_system_and_lanes() {
        let harness = setup_test_harness().await;

        // 1. system
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "system", "arguments": {}}
        });
        let resp = handle_message(&harness.api, &req.to_string())
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], Value::Null);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let val: Value = serde_json::from_str(text).unwrap();
        assert_eq!(val["principal"], "agent-a");
        assert_eq!(val["tenant_id"], "local");

        // 2. list_lanes
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "list_lanes", "arguments": {}}
        });
        let resp = handle_message(&harness.api, &req.to_string())
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], Value::Null);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let val: Value = serde_json::from_str(text).unwrap();
        assert_eq!(val["items"].as_array().unwrap().len(), 2);

        // 3. inspect_lane
        let req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "inspect_lane",
                "arguments": {"lane_id": "lane-one"}
            }
        });
        let resp = handle_message(&harness.api, &req.to_string())
            .await
            .unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let val: Value = serde_json::from_str(text).unwrap();
        assert_eq!(val["id"], "lane-one");
        assert_eq!(val["project"], "app");
        assert_eq!(val["environment"], "dev");
    }

    #[tokio::test]
    async fn test_tools_call_release_guide_two_modes_and_validation() {
        let harness = setup_test_harness().await;

        // 1. by lane_id
        let req = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "get_release_guide",
                "arguments": {"lane_id": "lane-one"}
            }
        });
        let resp = handle_message(&harness.api, &req.to_string())
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], Value::Null);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let val: Value = serde_json::from_str(text).unwrap();
        assert_eq!(val["lane_id"], "lane-one");
        let sections = val["sections"].as_array().unwrap();
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0]["name"], "project");
        assert_eq!(sections[1]["name"], "dev");
        assert_eq!(sections[2]["name"], "app");

        // 2. by project
        let req = json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "get_release_guide",
                "arguments": {"project": "app"}
            }
        });
        let resp = handle_message(&harness.api, &req.to_string())
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], Value::Null);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let val: Value = serde_json::from_str(text).unwrap();
        assert_eq!(val["project"], "app");
        assert_eq!(
            val["overview"]["content"],
            "# Project Overview\nGuide for all."
        );

        // 3. invalid arguments: both lane_id and project provided
        let req = json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/call",
            "params": {
                "name": "get_release_guide",
                "arguments": {"lane_id": "lane-one", "project": "app"}
            }
        });
        let resp = handle_message(&harness.api, &req.to_string())
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let val: Value = serde_json::from_str(text).unwrap();
        assert_eq!(val["code"], "invalid_arguments");

        // 4. invalid arguments: neither lane_id nor project provided
        let req = json!({
            "jsonrpc": "2.0",
            "id": 13,
            "method": "tools/call",
            "params": {
                "name": "get_release_guide",
                "arguments": {}
            }
        });
        let resp = handle_message(&harness.api, &req.to_string())
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let val: Value = serde_json::from_str(text).unwrap();
        assert_eq!(val["code"], "invalid_arguments");
    }

    #[tokio::test]
    async fn test_tools_call_submit_and_task_idempotency_consistency() {
        let harness = setup_test_harness().await;

        // 1. Submit with task_id: verify idempotency key matches task_idempotency_key
        let submit_req = json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "tools/call",
            "params": {
                "name": "submit_request",
                "arguments": {
                    "lane_id": "lane-one",
                    "revision": harness.revision,
                    "components": ["app"],
                    "task_id": "feature-101",
                    "reason": "mcp submission"
                }
            }
        });
        let resp = handle_message(&harness.api, &submit_req.to_string())
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], Value::Null);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let val: Value = serde_json::from_str(text).unwrap();
        let request_id = val["id"].as_str().unwrap();
        let key = val["idempotency_key"].as_str().unwrap();
        assert!(key.starts_with("task-"));

        // Derive expected key using shared function
        let canonical_intent = json!({
            "lane_id": "lane-one",
            "revision": harness.revision,
            "components": ["app"],
            "coverage_mode": "exact",
            "priority": "normal",
            "deadline_at": Value::Null,
            "reason": "mcp submission",
            "task_id": "feature-101"
        });
        let expected_key = crate::client::task_idempotency_key(&canonical_intent);
        assert_eq!(key, expected_key);

        // 2. Replay with identical intent and task_id: returns same key and reused: true
        let replay_resp = handle_message(&harness.api, &submit_req.to_string())
            .await
            .unwrap();
        let text2 = replay_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        let val2: Value = serde_json::from_str(text2).unwrap();
        assert_eq!(val2["id"], request_id);
        assert_eq!(val2["idempotency_key"], key);
        assert_eq!(val2["reused"], true);

        // 3. Submit via project + environment instead of lane_id
        let proj_submit_req = json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "tools/call",
            "params": {
                "name": "submit_request",
                "arguments": {
                    "project": "app",
                    "environment": "dev",
                    "revision": harness.revision,
                    "components": ["app"],
                    "task_id": "feature-101",
                    "reason": "mcp submission"
                }
            }
        });
        let resp3 = handle_message(&harness.api, &proj_submit_req.to_string())
            .await
            .unwrap();
        let text3 = resp3["result"]["content"][0]["text"].as_str().unwrap();
        let val3: Value = serde_json::from_str(text3).unwrap();
        // Because project "app" + environment "dev" canonicalizes to "lane-one",
        // with the same intent it should resolve to the same task key and reuse!
        assert_eq!(val3["id"], request_id);
        assert_eq!(val3["idempotency_key"], key);
        assert_eq!(val3["reused"], true);

        // 4. Submit without task_id: generates random UUID key
        let no_task_req = json!({
            "jsonrpc": "2.0",
            "id": 22,
            "method": "tools/call",
            "params": {
                "name": "submit_request",
                "arguments": {
                    "lane_id": "lane-one",
                    "revision": harness.revision,
                    "components": ["app"]
                }
            }
        });
        let resp4 = handle_message(&harness.api, &no_task_req.to_string())
            .await
            .unwrap();
        let text4 = resp4["result"]["content"][0]["text"].as_str().unwrap();
        let val4: Value = serde_json::from_str(text4).unwrap();
        let random_key = val4["idempotency_key"].as_str().unwrap();
        assert!(!random_key.starts_with("task-"));
        assert!(uuid::Uuid::parse_str(random_key).is_ok());
    }

    #[tokio::test]
    async fn test_tools_call_wait_request_timeout_and_bounds() {
        let harness = setup_test_harness().await;

        // Submit a request first
        let submit_req = json!({
            "jsonrpc": "2.0",
            "id": 30,
            "method": "tools/call",
            "params": {
                "name": "submit_request",
                "arguments": {
                    "lane_id": "lane-two",
                    "revision": harness.revision,
                    "components": ["app"]
                }
            }
        });
        let resp = handle_message(&harness.api, &submit_req.to_string())
            .await
            .unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let val: Value = serde_json::from_str(text).unwrap();
        let request_id = val["id"].as_str().unwrap();

        // 1. wait_request with short timeout (1 sec, poll 0.1 sec)
        // Since scripted execution is not ticked here, it will timeout locally
        let wait_req = json!({
            "jsonrpc": "2.0",
            "id": 31,
            "method": "tools/call",
            "params": {
                "name": "wait_request",
                "arguments": {
                    "request_id": request_id,
                    "timeout_seconds": 1,
                    "poll_seconds": 0.1
                }
            }
        });
        let wait_resp = handle_message(&harness.api, &wait_req.to_string())
            .await
            .unwrap();
        assert_eq!(wait_resp["result"]["isError"], Value::Null);
        let wait_text = wait_resp["result"]["content"][0]["text"].as_str().unwrap();
        let wait_val: Value = serde_json::from_str(wait_text).unwrap();
        assert_eq!(wait_val["id"], request_id);
        assert_eq!(wait_val["local_wait_timeout"], true);

        // 2. timeout_seconds exceeds 600 seconds -> invalid_arguments
        let bad_wait_req = json!({
            "jsonrpc": "2.0",
            "id": 32,
            "method": "tools/call",
            "params": {
                "name": "wait_request",
                "arguments": {
                    "request_id": request_id,
                    "timeout_seconds": 601
                }
            }
        });
        let bad_resp = handle_message(&harness.api, &bad_wait_req.to_string())
            .await
            .unwrap();
        assert_eq!(bad_resp["result"]["isError"], true);
        let bad_text = bad_resp["result"]["content"][0]["text"].as_str().unwrap();
        let bad_val: Value = serde_json::from_str(bad_text).unwrap();
        assert_eq!(bad_val["code"], "invalid_arguments");

        // 3. poll_seconds <= 0.0 -> invalid_arguments
        let bad_poll_req = json!({
            "jsonrpc": "2.0",
            "id": 33,
            "method": "tools/call",
            "params": {
                "name": "wait_request",
                "arguments": {
                    "request_id": request_id,
                    "poll_seconds": 0.0
                }
            }
        });
        let bad_poll_resp = handle_message(&harness.api, &bad_poll_req.to_string())
            .await
            .unwrap();
        assert_eq!(bad_poll_resp["result"]["isError"], true);
        let bad_poll_val: Value = serde_json::from_str(
            bad_poll_resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(bad_poll_val["code"], "invalid_arguments");
    }

    #[tokio::test]
    async fn test_tools_call_error_transparency_and_mutations() {
        let harness = setup_test_harness().await;

        // 1. Error transparency on not found request (valid UUID that does not exist)
        let non_existent_uuid = uuid::Uuid::new_v4().to_string();
        let get_req = json!({
            "jsonrpc": "2.0",
            "id": 40,
            "method": "tools/call",
            "params": {
                "name": "get_request",
                "arguments": {"request_id": non_existent_uuid}
            }
        });
        let resp = handle_message(&harness.api, &get_req.to_string())
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        let err_text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let err_val: Value = serde_json::from_str(err_text).unwrap();
        assert_eq!(err_val["code"], "not_found");
        assert_eq!(err_val["retryable"], false);

        // Also test invalid ID format
        let bad_id_req = json!({
            "jsonrpc": "2.0",
            "id": 401,
            "method": "tools/call",
            "params": {
                "name": "get_request",
                "arguments": {"request_id": "not-a-uuid"}
            }
        });
        let bad_id_resp = handle_message(&harness.api, &bad_id_req.to_string())
            .await
            .unwrap();
        assert_eq!(bad_id_resp["result"]["isError"], true);
        let bad_id_val: Value = serde_json::from_str(
            bad_id_resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(bad_id_val["code"], "invalid_id");

        // 2. Submit, withdraw, retry, raise_priority
        let submit_req = json!({
            "jsonrpc": "2.0",
            "id": 41,
            "method": "tools/call",
            "params": {
                "name": "submit_request",
                "arguments": {
                    "lane_id": "lane-one",
                    "revision": harness.revision,
                    "components": ["app"]
                }
            }
        });
        let resp = handle_message(&harness.api, &submit_req.to_string())
            .await
            .unwrap();
        let val: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let request_id = val["id"].as_str().unwrap();

        // Raise priority while request is waiting
        let prio_req = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "tools/call",
            "params": {
                "name": "raise_priority",
                "arguments": {
                    "request_id": request_id,
                    "priority": "urgent",
                    "reason": "incident hotfix"
                }
            }
        });
        let p_resp = handle_message(&harness.api, &prio_req.to_string())
            .await
            .unwrap();
        assert_eq!(p_resp["result"]["isError"], Value::Null);
        let p_val: Value =
            serde_json::from_str(p_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(p_val["priority"], "urgent");

        // Withdraw the request
        let withdraw_req = json!({
            "jsonrpc": "2.0",
            "id": 43,
            "method": "tools/call",
            "params": {
                "name": "withdraw_request",
                "arguments": {"request_id": request_id}
            }
        });
        let w_resp = handle_message(&harness.api, &withdraw_req.to_string())
            .await
            .unwrap();
        assert_eq!(w_resp["result"]["isError"], Value::Null);
        let w_val: Value =
            serde_json::from_str(w_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(w_val["status"], "withdrawn");

        // Retry withdrawn request fails with not_retryable domain error (transparent isError)
        let retry_req = json!({
            "jsonrpc": "2.0",
            "id": 44,
            "method": "tools/call",
            "params": {
                "name": "retry_request",
                "arguments": {"request_id": request_id}
            }
        });
        let r_resp = handle_message(&harness.api, &retry_req.to_string())
            .await
            .unwrap();
        assert_eq!(r_resp["result"]["isError"], true);
        let r_val: Value =
            serde_json::from_str(r_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(r_val["code"], "not_retryable");

        // Invalid priority argument
        let bad_prio_req = json!({
            "jsonrpc": "2.0",
            "id": 45,
            "method": "tools/call",
            "params": {
                "name": "raise_priority",
                "arguments": {
                    "request_id": request_id,
                    "priority": "highest"
                }
            }
        });
        let bp_resp = handle_message(&harness.api, &bad_prio_req.to_string())
            .await
            .unwrap();
        assert_eq!(bp_resp["result"]["isError"], true);
    }

    #[tokio::test]
    async fn test_tools_call_events_and_logs() {
        let harness = setup_test_harness().await;

        // get_events
        let events_req = json!({
            "jsonrpc": "2.0",
            "id": 50,
            "method": "tools/call",
            "params": {
                "name": "get_events",
                "arguments": {"lane_id": "lane-one"}
            }
        });
        let resp = handle_message(&harness.api, &events_req.to_string())
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], Value::Null);
        let val: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(val["items"].is_array());

        // get_logs with invalid arguments (neither run_id nor request_id)
        let bad_logs_req = json!({
            "jsonrpc": "2.0",
            "id": 51,
            "method": "tools/call",
            "params": {
                "name": "get_logs",
                "arguments": {}
            }
        });
        let resp = handle_message(&harness.api, &bad_logs_req.to_string())
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        let val: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(val["code"], "invalid_arguments");
    }
}
