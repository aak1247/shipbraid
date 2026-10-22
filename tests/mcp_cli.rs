use serde_json::{Value, json};
use shipbraid::api::router;
use shipbraid::client::{Api, SubmitParams, execute_submit, task_idempotency_key};
use shipbraid::config::{Config, GuideConfig, LaneConfig, LaneExecutorConfig, TokenConfig};
use shipbraid::service::Service;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write as StdWrite};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command as StdCommand, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
use tokio::net::TcpListener;
use tokio::process::{
    Child as TokioChild, ChildStdin as TokioStdin, ChildStdout as TokioStdout,
    Command as TokioCommand,
};
use uuid::Uuid;

const BINARY: &str = env!("CARGO_BIN_EXE_shipbraid");

struct SyncMcpClient {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
}

impl SyncMcpClient {
    fn spawn_bare() -> Self {
        let mut child = StdCommand::new(BINARY)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bare mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let reader = BufReader::new(stdout);
        Self {
            child,
            stdin,
            reader,
        }
    }

    fn send_line(&mut self, text: &str) {
        writeln!(self.stdin, "{text}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn send_val(&mut self, val: &Value) {
        self.send_line(&val.to_string());
    }

    fn read_val(&mut self) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("invalid JSON line: {line:?}: {e}"))
    }

    fn close(mut self) {
        drop(self.stdin);
        let status = self.child.wait().unwrap();
        assert!(status.success());
    }
}

struct AsyncMcpClient {
    child: TokioChild,
    stdin: TokioStdin,
    reader: TokioBufReader<TokioStdout>,
}

impl AsyncMcpClient {
    fn spawn(url: &str, token: &str) -> Self {
        let mut child = TokioCommand::new(BINARY)
            .args(["--url", url, "--token", token, "mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn async shipbraid mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let reader = TokioBufReader::new(stdout);
        Self {
            child,
            stdin,
            reader,
        }
    }

    async fn send_line(&mut self, text: &str) {
        self.stdin.write_all(text.as_bytes()).await.unwrap();
        self.stdin.write_all(b"\n").await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn send_val(&mut self, val: &Value) {
        self.send_line(&val.to_string()).await;
    }

    async fn read_val(&mut self) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("invalid JSON line: {line:?}: {e}"))
    }

    async fn call_tool(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": arguments,
            }
        });
        self.send_val(&req).await;
        self.read_val().await
    }

    async fn close(mut self) {
        drop(self.stdin);
        let status = self.child.wait().await.unwrap();
        assert!(status.success());
    }
}

fn git_cmd(dir: &Path, args: &[&str]) -> String {
    let output = StdCommand::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git output");
    assert!(
        output.status.success(),
        "git command failed: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct TestServer {
    _temp: TempDir,
    base_url: String,
    token_full: String,
    token_limited: String,
    token_executor: String,
    revision: String,
    _service: Arc<Service>,
    scheduler_abort: tokio::task::AbortHandle,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.scheduler_abort.abort();
    }
}

async fn setup_test_server() -> TestServer {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    fs::create_dir(&work).unwrap();
    git_cmd(&work, &["init", "-b", "release"]);
    git_cmd(&work, &["config", "user.name", "MCP Test"]);
    git_cmd(&work, &["config", "user.email", "mcp@example.invalid"]);
    fs::write(work.join("app.txt"), "mcp fixture content").unwrap();
    git_cmd(&work, &["add", "app.txt"]);
    git_cmd(&work, &["commit", "-m", "fixture commit"]);
    let revision = git_cmd(&work, &["rev-parse", "HEAD"]);

    let mirror = temp.path().join("mirror.git");
    assert!(
        StdCommand::new("git")
            .args([
                "clone",
                "--mirror",
                work.to_str().unwrap(),
                mirror.to_str().unwrap()
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );

    let guides_dir = temp.path().join("guides");
    fs::create_dir(&guides_dir).unwrap();
    let overview_file = guides_dir.join("overview.md");
    fs::write(&overview_file, "# Project Overview\nGuide for all.").unwrap();
    let dev_file = guides_dir.join("dev.md");
    fs::write(&dev_file, "# Dev Guide\nGuide for dev environment.").unwrap();
    let app_file = guides_dir.join("app.md");
    fs::write(&app_file, "# App Guide\nGuide for app component.").unwrap();

    let mut comp_map = BTreeMap::new();
    comp_map.insert("app".into(), app_file);

    let token_full = format!("token-full-{}", Uuid::new_v4());
    let token_limited = format!("token-limited-{}", Uuid::new_v4());
    let token_executor = format!("token-exec-{}", Uuid::new_v4());

    let env_full = format!("SHIPBRAID_MCP_T_{}_FULL", Uuid::new_v4().simple());
    let env_limited = format!("SHIPBRAID_MCP_T_{}_LIM", Uuid::new_v4().simple());
    let env_exec = format!("SHIPBRAID_MCP_T_{}_EXEC", Uuid::new_v4().simple());

    unsafe {
        std::env::set_var(&env_full, &token_full);
        std::env::set_var(&env_limited, &token_limited);
        std::env::set_var(&env_exec, &token_executor);
    }

    let tokens = vec![
        TokenConfig {
            id: "principal-full".into(),
            token_env: env_full.clone(),
            lanes: vec!["lane-one".into(), "lane-two".into(), "lane-disabled".into()],
            projects: vec![],
            urgent: true,
            admin: false,
            executor: false,
        },
        TokenConfig {
            id: "principal-limited".into(),
            token_env: env_limited.clone(),
            lanes: vec!["lane-two".into()],
            projects: vec![],
            urgent: false,
            admin: false,
            executor: false,
        },
        TokenConfig {
            id: "executor".into(),
            token_env: env_exec.clone(),
            lanes: vec!["lane-one".into(), "lane-two".into(), "lane-disabled".into()],
            projects: vec![],
            urgent: false,
            admin: false,
            executor: true,
        },
    ];

    let lanes = vec![
        LaneConfig {
            id: "lane-one".into(),
            project: "app".into(),
            environment: "dev".into(),
            repository_id: "repo".into(),
            repository_path: mirror.clone(),
            release_ref: "refs/heads/release".into(),
            profile_digest: "p1".into(),
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
            profile_digest: "p2".into(),
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
        LaneConfig {
            id: "lane-disabled".into(),
            project: "app".into(),
            environment: "disabled".into(),
            repository_id: "repo".into(),
            repository_path: mirror.clone(),
            release_ref: "refs/heads/release".into(),
            profile_digest: "p3".into(),
            components: vec!["app".into()],
            allow_descendant: true,
            allow_preemption: true,
            failure_observe: None,
            resource_admission: None,
            request_parameters: Default::default(),
            enabled: false,
            executor: LaneExecutorConfig::Scripted,
            guide: None,
            credentials: None,
            hook_environment: None,
        },
    ];

    let config = Config {
        database_path: temp.path().join("ledger.db"),
        tokens,
        lanes,
        tick_ms: 20,
        scripted_phase_ms: 300,
        coalesce_ms: 20,
        ..Config::default()
    };

    let service = Service::open(config).await.unwrap();

    unsafe {
        std::env::remove_var(&env_full);
        std::env::remove_var(&env_limited);
        std::env::remove_var(&env_exec);
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");

    let app = router(service.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let sched_service = service.clone();
    let sched = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(20));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let _ = sched_service.scheduler_tick().await;
        }
    });

    TestServer {
        _temp: temp,
        base_url,
        token_full,
        token_limited,
        token_executor,
        revision,
        _service: service,
        scheduler_abort: sched.abort_handle(),
    }
}

#[test]
fn mcp_stdio_end_to_end_protocol_lifecycle() {
    let mut child = StdCommand::new(BINARY)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn shipbraid mcp");

    let mut stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = BufReader::new(stdout);

    // 1. Send initialize
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "clientInfo": {"name": "test-agent", "version": "1.0"}
        }
    });
    writeln!(stdin, "{}", init_req).unwrap();
    stdin.flush().unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let init_resp: Value = serde_json::from_str(&line).expect("valid JSON line");
    assert_eq!(init_resp["jsonrpc"], "2.0");
    assert_eq!(init_resp["id"], 1);
    assert_eq!(init_resp["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "shipbraid-mcp");

    // 2. Send initialized notification (must be ignored, no response)
    let note = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    writeln!(stdin, "{}", note).unwrap();
    stdin.flush().unwrap();

    // 3. Send tools/list
    let list_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list"
    });
    writeln!(stdin, "{}", list_req).unwrap();
    stdin.flush().unwrap();

    line.clear();
    reader.read_line(&mut line).unwrap();
    let list_resp: Value = serde_json::from_str(&line).expect("valid JSON line");
    assert_eq!(list_resp["id"], 2);
    let tools = list_resp["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 14);

    // 4. Send unknown method -> -32601
    let unknown_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "not_a_method"
    });
    writeln!(stdin, "{}", unknown_req).unwrap();
    stdin.flush().unwrap();

    line.clear();
    reader.read_line(&mut line).unwrap();
    let unknown_resp: Value = serde_json::from_str(&line).expect("valid JSON line");
    assert_eq!(unknown_resp["id"], 3);
    assert_eq!(unknown_resp["error"]["code"], -32601);

    // 5. Close stdin -> process should cleanly exit with code 0
    drop(stdin);
    let status = child.wait().expect("wait for child");
    assert!(status.success());
}

#[test]
fn test_f1_initialize_handshake_all_versions_and_fallback() {
    let mut client = SyncMcpClient::spawn_bare();

    // 1. Version 2025-06-18 echoed back as-is
    let req1 = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "clientInfo": {"name": "test-client", "version": "1.0.0"}
        }
    });
    client.send_val(&req1);
    let resp1 = client.read_val();
    assert_eq!(resp1["jsonrpc"], "2.0");
    assert_eq!(resp1["id"], 1);
    assert_eq!(resp1["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(resp1["result"]["serverInfo"]["name"], "shipbraid-mcp");
    assert_eq!(
        resp1["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );
    assert!(resp1["result"]["capabilities"]["tools"].is_object());

    // 2. Version 2024-11-05 echoed back as-is
    let req2 = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05"
        }
    });
    client.send_val(&req2);
    let resp2 = client.read_val();
    assert_eq!(resp2["id"], 2);
    assert_eq!(resp2["result"]["protocolVersion"], "2024-11-05");

    // 3. Unknown version (1999-01-01) -> falls back to server supported version (2025-06-18)
    let req3 = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "initialize",
        "params": {
            "protocolVersion": "1999-01-01"
        }
    });
    client.send_val(&req3);
    let resp3 = client.read_val();
    assert_eq!(resp3["id"], 3);
    assert_eq!(
        resp3["result"]["protocolVersion"], "2025-06-18",
        "Must negotiate to server supported default protocolVersion: 2025-06-18"
    );

    // 4. Omitted version -> defaults to 2025-06-18
    let req4 = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "initialize"
    });
    client.send_val(&req4);
    let resp4 = client.read_val();
    assert_eq!(resp4["id"], 4);
    assert_eq!(resp4["result"]["protocolVersion"], "2025-06-18");

    client.close();
}

#[test]
fn test_f1_tools_list_snapshot_and_strict_negative_assertions() {
    let mut client = SyncMcpClient::spawn_bare();

    let list_req = json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/list"
    });
    client.send_val(&list_req);
    let resp = client.read_val();
    assert_eq!(resp["id"], 10);

    let tools = resp["result"]["tools"].as_array().expect("tools list");
    // Assert exactly 14 tools
    assert_eq!(
        tools.len(),
        14,
        "MCP tool surface must expose exactly 14 tools, found {}",
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

    // Strict negative assertion: tool names AND descriptions must NOT contain any admin operations
    let prohibited_admin_keywords = [
        "drain",
        "resume",
        "unpause",
        "resolve_unknown",
        "resolve-unknown",
        "resources",
        "gc-plan",
    ];

    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let desc = tool["description"].as_str().unwrap().to_lowercase();

        for kw in &prohibited_admin_keywords {
            assert!(
                !name.to_lowercase().contains(kw),
                "Tool name '{name}' must not contain administrative keyword '{kw}'"
            );
            assert!(
                !desc.contains(kw),
                "Tool '{name}' description '{desc}' must not contain administrative keyword '{kw}'"
            );
        }

        assert_eq!(
            tool["inputSchema"]["type"], "object",
            "Tool '{name}' inputSchema must be type: object"
        );
    }

    client.close();
}

#[test]
fn test_f1_jsonrpc_protocol_errors_and_notifications_stdio() {
    let mut client = SyncMcpClient::spawn_bare();

    // 1. Bad JSON line -> -32700 Parse error
    client.send_line("{bad json line");
    let resp = client.read_val();
    assert_eq!(resp["jsonrpc"], "2.0");
    assert!(resp["id"].is_null());
    assert_eq!(resp["error"]["code"], -32700);

    // 2. Request non-object (JSON array) -> -32600 Invalid Request
    client.send_line("[1, 2, 3]");
    let resp = client.read_val();
    assert!(resp["id"].is_null());
    assert_eq!(resp["error"]["code"], -32600);

    // 3. Request non-object (primitive string) -> -32600 Invalid Request
    client.send_line("\"primitive json string\"");
    let resp = client.read_val();
    assert!(resp["id"].is_null());
    assert_eq!(resp["error"]["code"], -32600);

    // 4. Request object missing method -> -32600 Invalid Request
    client.send_val(&json!({
        "jsonrpc": "2.0",
        "id": 100
    }));
    let resp = client.read_val();
    assert_eq!(resp["id"], 100);
    assert_eq!(resp["error"]["code"], -32600);

    // 5. Unknown method -> -32601 Method not found
    client.send_val(&json!({
        "jsonrpc": "2.0",
        "id": 101,
        "method": "nonexistent_custom_method"
    }));
    let resp = client.read_val();
    assert_eq!(resp["id"], 101);
    assert_eq!(resp["error"]["code"], -32601);

    // 6. Notifications without id (notifications/initialized, notifications/cancelled) produce NO response lines
    client.send_val(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    }));
    client.send_val(&json!({
        "jsonrpc": "2.0",
        "id": Value::Null,
        "method": "notifications/cancelled",
        "params": {"requestId": 42}
    }));

    // Verify channel is clean by immediately issuing a regular request
    client.send_val(&json!({
        "jsonrpc": "2.0",
        "id": 102,
        "method": "tools/list"
    }));
    let resp = client.read_val();
    // Must directly receive response for id 102, confirming notifications produced zero output lines
    assert_eq!(resp["id"], 102);

    client.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_f1_tools_call_error_transparency_403_and_404() {
    let server = setup_test_server().await;

    // Use token_limited: only authorized for lane-two, NOT lane-one
    let mut client = AsyncMcpClient::spawn(&server.base_url, &server.token_limited);

    // 1. inspect_lane on unauthorized lane -> API 403 Forbidden
    let resp = client
        .call_tool(201, "inspect_lane", json!({"lane_id": "lane-one"}))
        .await;
    assert_eq!(resp["id"], 201);
    assert_eq!(resp["result"]["isError"], true);
    let content = resp["result"]["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();
    let err_val: Value = serde_json::from_str(text).expect("structured failure JSON");
    assert_eq!(err_val["code"], "forbidden");
    assert_eq!(err_val["http_status"], 403);
    assert_eq!(err_val["retryable"], false);

    // 2. get_release_guide on unauthorized lane -> API 403 Forbidden
    let resp = client
        .call_tool(202, "get_release_guide", json!({"lane_id": "lane-one"}))
        .await;
    assert_eq!(resp["id"], 202);
    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let err_val: Value = serde_json::from_str(text).unwrap();
    assert_eq!(err_val["code"], "forbidden");
    assert_eq!(err_val["http_status"], 403);

    // 3. get_request with non-existent UUID -> API 404 Not Found
    let missing_uuid = Uuid::new_v4().to_string();
    let resp = client
        .call_tool(203, "get_request", json!({"request_id": missing_uuid}))
        .await;
    assert_eq!(resp["id"], 203);
    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let err_val: Value = serde_json::from_str(text).unwrap();
    assert_eq!(err_val["code"], "not_found");
    assert_eq!(err_val["http_status"], 404);
    assert_eq!(err_val["retryable"], false);

    client.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_f1_submit_request_task_idempotency_byte_for_byte_consistency() {
    let server = setup_test_server().await;
    let api = Api::new(&server.base_url, Some(server.token_full.clone())).unwrap();

    // 1. Submit via CLI execute_submit
    let task_id = "task-sync-cross-interface";
    let submit_params = SubmitParams {
        lane_id: Some("lane-one".into()),
        project: None,
        environment: None,
        revision: server.revision.clone(),
        components: vec!["app".into()],
        coverage: Some("exact".into()),
        priority: Some("normal".into()),
        deadline_at: None,
        reason: Some("cli sync submission".into()),
        idempotency_key: None,
        task_id: Some(task_id.into()),
        session_id: Some("session-cli".into()),
        collaborators: vec![],
        parameters: None,
    };
    let (cli_val, cli_key) = execute_submit(&api, submit_params).await.unwrap();
    let request_id = cli_val["id"].as_str().unwrap();

    // Verify CLI derived idempotency key matches task_idempotency_key
    let canonical_intent = json!({
        "lane_id": "lane-one",
        "revision": server.revision,
        "components": ["app"],
        "coverage_mode": "exact",
        "priority": "normal",
        "deadline_at": Value::Null,
        "reason": "cli sync submission",
        "task_id": task_id
    });
    let expected_key = task_idempotency_key(&canonical_intent);
    assert_eq!(
        cli_key, expected_key,
        "CLI derived key must match task_idempotency_key byte-for-byte"
    );

    // 2. Submit via MCP stdio tools/call with the exact same intent
    let mut client = AsyncMcpClient::spawn(&server.base_url, &server.token_full);
    let mcp_resp = client
        .call_tool(
            301,
            "submit_request",
            json!({
                "lane_id": "lane-one",
                "revision": server.revision,
                "components": ["app"],
                "task_id": task_id,
                "reason": "cli sync submission"
            }),
        )
        .await;
    assert_eq!(mcp_resp["result"]["isError"], Value::Null);
    let mcp_text = mcp_resp["result"]["content"][0]["text"].as_str().unwrap();
    let mcp_val: Value = serde_json::from_str(mcp_text).unwrap();

    // Byte-for-byte comparison of derived idempotency key between CLI and MCP
    let mcp_key = mcp_val["idempotency_key"].as_str().unwrap();
    assert_eq!(
        mcp_key, cli_key,
        "MCP derived idempotency key must be byte-for-byte identical to CLI key"
    );
    assert_eq!(mcp_val["id"], request_id);
    assert_eq!(
        mcp_val["reused"], true,
        "Replay with same task_id must return reused: true"
    );

    // 3. Submit again via MCP with a DIFFERENT session_id: still returns reused: true and same key
    let mcp_resp2 = client
        .call_tool(
            302,
            "submit_request",
            json!({
                "lane_id": "lane-one",
                "revision": server.revision,
                "components": ["app"],
                "task_id": task_id,
                "session_id": "completely-different-session-id",
                "reason": "cli sync submission"
            }),
        )
        .await;
    let mcp_val2: Value =
        serde_json::from_str(mcp_resp2["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(mcp_val2["idempotency_key"], cli_key);
    assert_eq!(mcp_val2["id"], request_id);
    assert_eq!(mcp_val2["reused"], true);

    // 4. Submit via project + environment instead of lane_id: canonicalizes and reuses
    let mcp_resp3 = client
        .call_tool(
            303,
            "submit_request",
            json!({
                "project": "app",
                "environment": "dev",
                "revision": server.revision,
                "components": ["app"],
                "task_id": task_id,
                "reason": "cli sync submission"
            }),
        )
        .await;
    let mcp_val3: Value =
        serde_json::from_str(mcp_resp3["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(mcp_val3["idempotency_key"], cli_key);
    assert_eq!(mcp_val3["id"], request_id);
    assert_eq!(mcp_val3["reused"], true);

    client.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_f1_wait_request_terminal_states_and_timeout() {
    let server = setup_test_server().await;
    let mut client = AsyncMcpClient::spawn(&server.base_url, &server.token_full);

    // A. Terminal state 1: satisfied
    let sub1 = client
        .call_tool(
            401,
            "submit_request",
            json!({
                "lane_id": "lane-one",
                "revision": server.revision,
                "components": ["app"]
            }),
        )
        .await;
    let req1_id =
        serde_json::from_str::<Value>(sub1["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();

    // wait_request on req1: scheduler ticks and advances scripted execution to satisfied
    let wait1 = client
        .call_tool(
            402,
            "wait_request",
            json!({
                "request_id": req1_id,
                "timeout_seconds": 15,
                "poll_seconds": 0.1
            }),
        )
        .await;
    assert_eq!(wait1["result"]["isError"], Value::Null);
    let val1: Value =
        serde_json::from_str(wait1["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        val1["status"], "satisfied",
        "wait_request must return satisfied terminal status"
    );
    assert_eq!(val1["local_wait_timeout"], Value::Null);

    // B. Terminal state 2: needs_attention via injected executor failure
    let sub2 = client
        .call_tool(
            403,
            "submit_request",
            json!({
                "lane_id": "lane-two",
                "revision": server.revision,
                "components": ["app"]
            }),
        )
        .await;
    let req2_id =
        serde_json::from_str::<Value>(sub2["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();

    // Wait until run is dispatched
    let mut run_info = None;
    for _ in 0..100 {
        let r_resp = client
            .call_tool(404, "get_request", json!({"request_id": req2_id}))
            .await;
        let r_val: Value =
            serde_json::from_str(r_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        if let Some(run_id) = r_val.get("current_run_id").and_then(Value::as_str) {
            let run_resp = client
                .call_tool(405, "get_run", json!({"run_id": run_id}))
                .await;
            let run_val: Value =
                serde_json::from_str(run_resp["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            if let Some(eref) = run_val.get("executor_ref").and_then(Value::as_str) {
                run_info = Some((
                    run_id.to_owned(),
                    run_val["generation"].as_u64().unwrap(),
                    eref.to_owned(),
                ));
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    let (run_id, generation, eref) =
        run_info.expect("run must be dispatched and have executor_ref");

    // Inject failed event from executor
    let http = reqwest::Client::new();
    let fail_resp = http
        .post(format!("{}/v1/executor-events", server.base_url))
        .bearer_auth(&server.token_executor)
        .json(&json!({
            "run_id": run_id,
            "generation": generation,
            "executor_ref": eref,
            "event": "failed",
            "reason": "synthetic test failure",
            "environment_known_unchanged": true,
        }))
        .send()
        .await
        .unwrap();
    assert!(fail_resp.status().is_success());

    // wait_request on req2: returns needs_attention terminal status
    let wait2 = client
        .call_tool(
            406,
            "wait_request",
            json!({
                "request_id": req2_id,
                "timeout_seconds": 10,
                "poll_seconds": 0.1
            }),
        )
        .await;
    assert_eq!(wait2["result"]["isError"], Value::Null);
    let val2: Value =
        serde_json::from_str(wait2["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        val2["status"], "needs_attention",
        "wait_request must return needs_attention terminal status"
    );
    assert_eq!(val2["local_wait_timeout"], Value::Null);

    // C. Local wait timeout: timeout_seconds elapses, local_wait_timeout: true, server request NOT withdrawn
    let sub3 = client
        .call_tool(
            407,
            "submit_request",
            json!({
                "lane_id": "lane-disabled",
                "revision": server.revision,
                "components": ["app"]
            }),
        )
        .await;
    let req3_id =
        serde_json::from_str::<Value>(sub3["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();

    let wait3 = client
        .call_tool(
            408,
            "wait_request",
            json!({
                "request_id": req3_id,
                "timeout_seconds": 1,
                "poll_seconds": 0.1
            }),
        )
        .await;
    assert_eq!(wait3["result"]["isError"], Value::Null);
    let val3: Value =
        serde_json::from_str(wait3["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        val3["local_wait_timeout"], true,
        "Local timeout must attach local_wait_timeout: true"
    );

    // Verify request on coordinator remains waiting/active and is NOT withdrawn
    let get3 = client
        .call_tool(409, "get_request", json!({"request_id": req3_id}))
        .await;
    let val3_active: Value =
        serde_json::from_str(get3["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        val3_active["status"], "waiting",
        "Local timeout must not withdraw or cancel the server request"
    );
    assert_ne!(val3_active["status"], "withdrawn");

    // D. Argument validation: timeout_seconds > 600 -> invalid_arguments
    let wait_bad = client
        .call_tool(
            410,
            "wait_request",
            json!({
                "request_id": req3_id,
                "timeout_seconds": 601
            }),
        )
        .await;
    assert_eq!(wait_bad["result"]["isError"], true);
    let val_bad: Value =
        serde_json::from_str(wait_bad["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(val_bad["code"], "invalid_arguments");

    client.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_f1_get_release_guide_endpoint_routing() {
    let server = setup_test_server().await;
    let mut client = AsyncMcpClient::spawn(&server.base_url, &server.token_full);

    // 1. By lane_id -> routes to /v1/lanes/{id}/guide
    let resp1 = client
        .call_tool(
            501,
            "get_release_guide",
            json!({
                "lane_id": "lane-one"
            }),
        )
        .await;
    assert_eq!(resp1["result"]["isError"], Value::Null);
    let val1: Value =
        serde_json::from_str(resp1["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(val1["lane_id"], "lane-one");
    assert_eq!(val1["project"], "app");
    assert_eq!(val1["environment"], "dev");
    let sections = val1["sections"].as_array().expect("sections array");
    assert_eq!(sections.len(), 3);
    assert_eq!(sections[0]["name"], "project");
    assert_eq!(sections[1]["name"], "dev");
    assert_eq!(sections[2]["name"], "app");

    // 2. By project -> routes to /v1/guides/{project}
    let resp2 = client
        .call_tool(
            502,
            "get_release_guide",
            json!({
                "project": "app"
            }),
        )
        .await;
    assert_eq!(resp2["result"]["isError"], Value::Null);
    let val2: Value =
        serde_json::from_str(resp2["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(val2["project"], "app");
    assert!(
        val2["overview"]["content"]
            .as_str()
            .unwrap()
            .contains("# Project Overview")
    );
    let envs = val2["environments"].as_array().unwrap();
    assert_eq!(envs[0]["environment"], "dev");
    let comps = val2["components"].as_array().unwrap();
    assert_eq!(comps[0]["component"], "app");

    // 3. Mutually exclusive validation: both lane_id and project -> invalid_arguments
    let resp3 = client
        .call_tool(
            503,
            "get_release_guide",
            json!({
                "lane_id": "lane-one",
                "project": "app"
            }),
        )
        .await;
    assert_eq!(resp3["result"]["isError"], true);
    let val3: Value =
        serde_json::from_str(resp3["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(val3["code"], "invalid_arguments");

    // 4. Mutually exclusive validation: neither lane_id nor project -> invalid_arguments
    let resp4 = client.call_tool(504, "get_release_guide", json!({})).await;
    assert_eq!(resp4["result"]["isError"], true);
    let val4: Value =
        serde_json::from_str(resp4["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(val4["code"], "invalid_arguments");

    client.close().await;
}
