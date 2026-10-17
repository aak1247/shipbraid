use std::{
    env, fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::{Builder, TempDir};

const BINARY: &str = env!("CARGO_BIN_EXE_shipbraid");
const HOOK_SECRET: &str = "local-cli-private-hook-canary";
const CONFIG_SECRET: &str = "local-cli-private-config-path-canary";

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    config: PathBuf,
    database: PathBuf,
    mirror: PathBuf,
    revisions: [String; 2],
    listen: TcpListener,
    hook_script: PathBuf,
    dispatch_marker: PathBuf,
    dispatch_settled_marker: PathBuf,
    observe_mode: PathBuf,
    dispatch_behavior: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = Builder::new()
            .prefix("shipbraid-local-cli-private-config-path-canary-")
            .tempdir()
            .unwrap();
        let root = temp.path().to_path_buf();
        let source = root.join("source");
        let mirror = root.join("source.git");
        fs::create_dir(&source).unwrap();
        git(
            &["init", "-q", "-b", "main", source.to_str().unwrap()],
            &root,
        );

        let mut revisions = Vec::new();
        for index in 0..2 {
            fs::write(source.join("revision"), format!("synthetic {index}\n")).unwrap();
            git(
                &[
                    "-C",
                    source.to_str().unwrap(),
                    "-c",
                    "user.name=Local CLI fixture",
                    "-c",
                    "user.email=local-cli@example.invalid",
                    "add",
                    "revision",
                ],
                &root,
            );
            git(
                &[
                    "-C",
                    source.to_str().unwrap(),
                    "-c",
                    "user.name=Local CLI fixture",
                    "-c",
                    "user.email=local-cli@example.invalid",
                    "commit",
                    "-qm",
                    "synthetic fixture revision",
                ],
                &root,
            );
            revisions.push(git_output(
                &["-C", source.to_str().unwrap(), "rev-parse", "HEAD"],
                &root,
            ));
        }
        git(
            &[
                "clone",
                "-q",
                "--mirror",
                source.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
            &root,
        );

        let listen = TcpListener::bind("127.0.0.1:0").unwrap();
        let hook_script = root.join("hooks").join("reply.py");
        let dispatch_marker = root.join("dispatch-count.txt");
        let dispatch_settled_marker = root.join("dispatch-settled.txt");
        let observe_mode = root.join("observe-mode.txt");
        fs::create_dir_all(hook_script.parent().unwrap()).unwrap();
        fs::write(
            &hook_script,
            r#"import datetime
import json
from pathlib import Path
import sys
import time
import uuid

operation, marker_name, observe_name, dispatch_behavior, settled_name = sys.argv[1:]
marker = Path(marker_name)
observe_mode = Path(observe_name)
settled_marker = Path(settled_name)
envelope = json.load(sys.stdin)
run = envelope["run"]
target = run["target"]
executor_ref = "synthetic-" + run["id"]

if operation == "dispatch":
    with marker.open("a") as output:
        output.write("dispatch\n")
    if dispatch_behavior == "unknown":
        raise SystemExit("local-cli-private-hook-canary")
    if dispatch_behavior == "slow":
        time.sleep(1.3)
        settled_marker.write_text("settled\n")
    status = "started"
elif operation == "observe":
    if dispatch_behavior == "unknown":
        status = "unknown"
    else:
        status = "completed" if observe_mode.read_text().strip() == "complete" else "running"
elif operation == "verify":
    status = "verified"
else:
    raise SystemExit("unexpected hook operation")

reply = {
    "version": 1,
    "run_id": run["id"],
    "generation": run["generation"],
    "executor_ref": executor_ref,
    "status": status,
}
if operation == "verify":
    observed_at = datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")
    reply["receipt"] = {
        "id": str(uuid.uuid4()),
        "run_id": run["id"],
        "generation": run["generation"],
        "observed_at": observed_at,
        "source_revision": target["revision"],
        "component_digests": {name: "a" * 64 for name in target["components"]},
        "profile_digest": target["profile_digest"],
        "verification_report": "synthetic local CLI acceptance",
        "result": "success",
    }
print(json.dumps(reply))
"#,
        )
        .unwrap();
        fs::write(&observe_mode, "running\n").unwrap();

        let fixture = Self {
            config: root.join("config.toml"),
            database: root.join("state").join("coordinator.sqlite"),
            mirror,
            revisions: [revisions.remove(0), revisions.remove(0)],
            listen,
            hook_script,
            dispatch_marker,
            dispatch_settled_marker,
            observe_mode,
            dispatch_behavior: "started".to_owned(),
            _temp: temp,
            root,
        };
        fixture.write_config("local", &[("lane-one", "project-one", "dev")], false);
        fixture
    }

    fn write_config(&self, mode: &str, lanes: &[(&str, &str, &str)], hooks: bool) {
        let mut config = format!(
            "mode = {}\ndatabase_path = {}\nlisten = {}\nmax_active_runs = 1\ntick_ms = 5\nscripted_phase_ms = 2\ncancel_timeout_seconds = 2\ncoalesce_ms = 0\n",
            quote(mode),
            quote(self.database.to_str().unwrap()),
            quote(&self.listen.local_addr().unwrap().to_string()),
        );
        if mode == "server" {
            config.push_str(
                "\n[[tokens]]\nid = \"operator\"\ntoken_env = \"SHIPBRAID_LOCAL_CLI_PRIVATE_TOKEN_CANARY\"\nadmin = true\n",
            );
        }
        for (index, (id, project, environment)) in lanes.iter().enumerate() {
            config.push_str(&format!(
                "\n[[lanes]]\nid = {}\nproject = {}\nenvironment = {}\nrepository_id = {}\nrepository_path = {}\nrelease_ref = \"refs/heads/main\"\nprofile_digest = \"local-cli-profile-{index}\"\ncomponents = [\"app\", \"worker\"]\nallow_preemption = false\n",
                quote(id),
                quote(project),
                quote(environment),
                quote(&format!("repository-{index}")),
                quote(self.mirror.to_str().unwrap()),
            ));
            if hooks {
                config.push_str("\n[lanes.executor]\nkind = \"hooks\"\n");
                let script = quote(self.hook_script.to_str().unwrap());
                let marker = quote(self.dispatch_marker.to_str().unwrap());
                let settled = quote(self.dispatch_settled_marker.to_str().unwrap());
                let observe = quote(self.observe_mode.to_str().unwrap());
                let behavior = quote(&self.dispatch_behavior);
                for operation in ["dispatch", "observe", "verify"] {
                    let timeout_ms = if self.dispatch_behavior == "slow" && operation == "dispatch"
                    {
                        5000
                    } else {
                        1000
                    };
                    config.push_str(&format!(
                        "\n[lanes.executor.{operation}]\nkind = \"script\"\npath = {script}\ninterpreter = \"python3\"\nargs = [{}, {marker}, {observe}, {behavior}, {settled}]\ntimeout_ms = {timeout_ms}\n",
                        quote(operation)
                    ));
                }
            }
        }
        fs::write(&self.config, config).unwrap();
    }

    fn run_args(&self, extras: &[&str]) -> Vec<String> {
        let mut args = vec![
            "--json".to_owned(),
            "run".to_owned(),
            "--config".to_owned(),
            self.config.to_string_lossy().into_owned(),
            "--revision".to_owned(),
            self.revisions[0].clone(),
            "--components".to_owned(),
            "app".to_owned(),
        ];
        args.extend(extras.iter().map(|value| (*value).to_owned()));
        args
    }

    fn cli_environment(&self, command: &mut Command) {
        command
            .env_clear()
            .env("PATH", env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
    }

    fn run_cli(&self, args: &[String]) -> Output {
        let mut command = Command::new(BINARY);
        command.args(args);
        self.cli_environment(&mut command);
        command.output().unwrap()
    }

    fn spawn_cli(&self, args: &[String], stderr_path: &Path) -> RunningCli {
        let mut command = Command::new(BINARY);
        command.args(args).stdout(Stdio::piped());
        self.cli_environment(&mut command);
        let stderr = fs::File::create(stderr_path).unwrap();
        let child = command.stderr(Stdio::from(stderr)).spawn().unwrap();
        RunningCli {
            child: Some(child),
            stderr_path: stderr_path.to_path_buf(),
        }
    }
}

struct RunningCli {
    child: Option<Child>,
    stderr_path: PathBuf,
}

impl RunningCli {
    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().unwrap()
    }

    fn stderr(&self) -> String {
        fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    fn send_term(&mut self) {
        #[cfg(unix)]
        {
            let pid = self.child_mut().id().to_string();
            let status = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
            assert!(status.success(), "could not send SIGTERM to local CLI");
        }
    }

    fn finish(mut self) -> Output {
        let mut child = self.child.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                return child.wait_with_output().unwrap();
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        child.wait_with_output().unwrap()
    }
}

impl Drop for RunningCli {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut()
            && child.try_wait().unwrap().is_none()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn quote(value: &str) -> String {
    serde_json::to_string(value).unwrap()
}

fn git(arguments: &[&str], current_dir: &Path) {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(current_dir)
        .env_clear()
        .env("PATH", env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "synthetic Git fixture setup failed: {}",
        output.status
    );
}

fn git_output(arguments: &[&str], current_dir: &Path) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(current_dir)
        .env_clear()
        .env("PATH", env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(output.status.success(), "synthetic Git lookup failed");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn json_report(output: &Output, code: i32) -> Value {
    assert_eq!(output.status.code(), Some(code));
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "expected a machine-readable local CLI result; stdout={:?}, stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn reported_request_id(report: &Value) -> &str {
    report["request_id"]
        .as_str()
        .or_else(|| report["request"]["id"].as_str())
        .expect("local report should retain the request identity")
}

fn error_code(report: &Value) -> &str {
    report["error"]["code"]
        .as_str()
        .or_else(|| report["code"].as_str())
        .expect("local CLI error should have a stable code")
}

fn assert_secret_free(output: &Output, secrets: &[&str]) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for secret in secrets {
        assert!(
            !text.contains(secret),
            "local CLI output disclosed {secret:?}"
        );
    }
}

fn wait_for_dispatch(child: &mut RunningCli, marker: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.child_mut().try_wait().unwrap().is_some() {
            panic!(
                "local CLI exited before dispatch marker appeared: {}",
                child.stderr()
            );
        }
        if fs::read_to_string(marker).is_ok_and(|contents| contents.lines().count() > 0) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "local CLI did not dispatch before timeout: {}",
        child.stderr()
    );
}

#[test]
fn local_run_completes_offline_and_replays_or_rejects_the_same_key() {
    let fixture = Fixture::new();
    let mut args = fixture.run_args(&["--idempotency-key", "same-local-intent"]);
    args.splice(
        1..1,
        [
            "--url".to_owned(),
            "file:///local-cli-invalid-url-canary".to_owned(),
            "--token-file".to_owned(),
            fixture
                .root
                .join("local-cli-missing-token-file-canary")
                .to_string_lossy()
                .into_owned(),
        ],
    );
    args.extend(["--timeout-seconds".to_owned(), "10".to_owned()]);
    let output = fixture.run_cli(&args);
    let report = json_report(&output, 0);
    let request_id = reported_request_id(&report).to_owned();
    assert_eq!(report["mode"], "local");
    assert_eq!(report["request"]["status"], "satisfied");
    assert_eq!(report["request"]["evidence"], "matching_receipt");
    assert!(!report["receipt"]["id"].is_null());
    assert_eq!(report["run"]["id"], report["receipt"]["run_id"]);
    assert_eq!(report["local_wait_timeout"], false);
    assert_eq!(report["interrupted"], false);
    assert!(String::from_utf8_lossy(&output.stderr).contains("accepted request_id="));
    assert!(String::from_utf8_lossy(&output.stderr).contains("same-local-intent"));
    assert_secret_free(
        &output,
        &[
            "local-cli-invalid-url-canary",
            "local-cli-missing-token-file-canary",
            CONFIG_SECRET,
            HOOK_SECRET,
        ],
    );

    let replay = fixture.run_cli(&fixture.run_args(&[
        "--idempotency-key",
        "same-local-intent",
        "--timeout-seconds",
        "0",
    ]));
    let replay_report = json_report(&replay, 0);
    assert_eq!(reported_request_id(&replay_report), request_id);
    assert_eq!(replay_report["reused"], true);

    let mut conflict_args = fixture.run_args(&[
        "--idempotency-key",
        "same-local-intent",
        "--timeout-seconds",
        "0",
    ]);
    let revision_index = conflict_args
        .iter()
        .position(|argument| argument == "--revision")
        .unwrap();
    conflict_args[revision_index + 1] = fixture.revisions[1].clone();
    let conflict = fixture.run_cli(&conflict_args);
    let conflict_report = json_report(&conflict, 1);
    assert_eq!(error_code(&conflict_report), "idempotency_conflict");
    assert_secret_free(
        &conflict,
        &["same-local-intent", CONFIG_SECRET, HOOK_SECRET],
    );
}

#[test]
fn zero_timeout_persists_without_a_run_and_resume_completes_that_request() {
    let fixture = Fixture::new();
    let accepted = fixture.run_cli(&fixture.run_args(&[
        "--idempotency-key",
        "resume-local-request",
        "--timeout-seconds",
        "0",
    ]));
    let accepted_report = json_report(&accepted, 124);
    let id = reported_request_id(&accepted_report).to_owned();
    assert!(accepted_report["run"].is_null());
    assert_eq!(accepted_report["local_wait_timeout"], true);
    assert!(fixture.database.exists());

    let resumed = fixture.run_cli(&[
        "--json".to_owned(),
        "run".to_owned(),
        "--config".to_owned(),
        fixture.config.to_string_lossy().into_owned(),
        "--resume".to_owned(),
        id.clone(),
        "--timeout-seconds".to_owned(),
        "10".to_owned(),
    ]);
    let resumed_report = json_report(&resumed, 0);
    assert_eq!(reported_request_id(&resumed_report), id);
    assert_eq!(resumed_report["request"]["status"], "satisfied");
    assert_eq!(resumed_report["request"]["evidence"], "matching_receipt");
    assert_eq!(
        resumed_report["run"]["id"],
        resumed_report["receipt"]["run_id"]
    );
}

#[test]
fn local_mode_requires_explicit_lane_for_multiple_lanes_and_rejects_cross_project_config() {
    let fixture = Fixture::new();
    fixture.write_config(
        "local",
        &[
            ("lane-one", "project-one", "dev"),
            ("lane-two", "project-one", "staging"),
        ],
        false,
    );
    let ambiguous = fixture.run_cli(&fixture.run_args(&["--timeout-seconds", "0"]));
    let ambiguous_report = json_report(&ambiguous, 1);
    assert_eq!(error_code(&ambiguous_report), "lane_required");
    assert_secret_free(&ambiguous, &[CONFIG_SECRET, HOOK_SECRET]);

    let selected =
        fixture.run_cli(&fixture.run_args(&["--lane", "lane-two", "--timeout-seconds", "10"]));
    let selected_report = json_report(&selected, 0);
    assert_eq!(selected_report["request"]["lane_id"], "lane-two");

    fixture.write_config(
        "local",
        &[
            ("lane-one", "project-one", "dev"),
            ("other-project", "project-two", "prod"),
        ],
        false,
    );
    let cross_project = fixture.run_cli(&fixture.run_args(&["--timeout-seconds", "0"]));
    let cross_project_report = json_report(&cross_project, 1);
    assert_eq!(error_code(&cross_project_report), "local_config");
    assert_secret_free(&cross_project, &["project-two", CONFIG_SECRET, HOOK_SECRET]);
}

#[test]
fn local_run_holds_one_ledger_lock_and_resumes_after_sigterm_without_redispatch() {
    let mut fixture = Fixture::new();
    fixture.dispatch_behavior = "started".to_owned();
    fixture.write_config("local", &[("lane-one", "project-one", "dev")], true);
    let stderr_path = fixture.root.join("first.stderr");
    let mut first = fixture.spawn_cli(
        &fixture.run_args(&[
            "--timeout-seconds",
            "30",
            "--idempotency-key",
            "signal-resume-key",
        ]),
        &stderr_path,
    );
    wait_for_dispatch(
        &mut first,
        &fixture.dispatch_marker,
        Duration::from_secs(10),
    );

    let second = fixture.run_cli(&fixture.run_args(&[
        "--timeout-seconds",
        "0",
        "--idempotency-key",
        "must-not-be-accepted-under-lock",
    ]));
    let second_report = json_report(&second, 1);
    assert_eq!(error_code(&second_report), "ledger_locked");
    assert_secret_free(&second, &["must-not-be-accepted-under-lock", HOOK_SECRET]);

    first.send_term();
    let interrupted = first.finish();
    let interrupted_report = json_report(&interrupted, 130);
    let request_id = reported_request_id(&interrupted_report).to_owned();
    assert_eq!(interrupted_report["interrupted"], true);
    assert_eq!(
        fs::read_to_string(&fixture.dispatch_marker)
            .unwrap()
            .lines()
            .count(),
        1
    );

    fs::write(&fixture.observe_mode, "complete\n").unwrap();
    let resumed = fixture.run_cli(&[
        "--json".to_owned(),
        "run".to_owned(),
        "--config".to_owned(),
        fixture.config.to_string_lossy().into_owned(),
        "--resume".to_owned(),
        request_id.clone(),
        "--timeout-seconds".to_owned(),
        "10".to_owned(),
    ]);
    let resumed_report = json_report(&resumed, 0);
    assert_eq!(reported_request_id(&resumed_report), request_id);
    assert_eq!(resumed_report["request"]["status"], "satisfied");
    assert_eq!(
        fs::read_to_string(&fixture.dispatch_marker)
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn slow_dispatch_handoff_finishes_before_sigterm_or_wait_timeout_returns() {
    let mut interrupted_fixture = Fixture::new();
    interrupted_fixture.dispatch_behavior = "slow".to_owned();
    interrupted_fixture.write_config("local", &[("lane-one", "project-one", "dev")], true);
    let stderr_path = interrupted_fixture.root.join("slow-interrupted.stderr");
    let mut child = interrupted_fixture.spawn_cli(
        &interrupted_fixture.run_args(&["--timeout-seconds", "30"]),
        &stderr_path,
    );
    wait_for_dispatch(
        &mut child,
        &interrupted_fixture.dispatch_marker,
        Duration::from_secs(10),
    );
    child.send_term();
    let interrupted = child.finish();
    let interrupted_report = json_report(&interrupted, 130);
    assert_eq!(interrupted_report["interrupted"], true);
    assert_eq!(interrupted_report["run"]["state"], "running");
    assert!(interrupted_fixture.dispatch_settled_marker.exists());
    assert!(
        interrupted_report["scope"]
            .as_str()
            .is_some_and(|scope| scope.contains("external execution may continue"))
    );
    assert_eq!(
        fs::read_to_string(&interrupted_fixture.dispatch_marker)
            .unwrap()
            .lines()
            .count(),
        1
    );

    let mut timeout_fixture = Fixture::new();
    timeout_fixture.dispatch_behavior = "slow".to_owned();
    timeout_fixture.write_config("local", &[("lane-one", "project-one", "dev")], true);
    let started = Instant::now();
    let timed_out = timeout_fixture.run_cli(&timeout_fixture.run_args(&["--timeout-seconds", "1"]));
    let elapsed = started.elapsed();
    let timeout_report = json_report(&timed_out, 124);
    assert!(elapsed >= Duration::from_millis(1200));
    assert_eq!(timeout_report["local_wait_timeout"], true);
    assert_eq!(timeout_report["run"]["state"], "running");
    assert!(timeout_fixture.dispatch_settled_marker.exists());
    assert!(
        timeout_report["scope"]
            .as_str()
            .is_some_and(|scope| scope.contains("external execution may continue"))
    );
    assert_eq!(
        fs::read_to_string(&timeout_fixture.dispatch_marker)
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn unknown_dispatch_is_retained_and_retry_does_not_replay_it() {
    let mut fixture = Fixture::new();
    fixture.dispatch_behavior = "unknown".to_owned();
    fixture.write_config("local", &[("lane-one", "project-one", "dev")], true);
    let first = fixture.run_cli(&fixture.run_args(&["--timeout-seconds", "10"]));
    let unknown = json_report(&first, 2);
    let id = reported_request_id(&unknown).to_owned();
    assert_eq!(unknown["run"]["state"], "unknown");
    assert_secret_free(&first, &[HOOK_SECRET]);
    assert_eq!(
        fs::read_to_string(&fixture.dispatch_marker)
            .unwrap()
            .lines()
            .count(),
        1
    );

    let resumed = fixture.run_cli(&[
        "--json".to_owned(),
        "run".to_owned(),
        "--config".to_owned(),
        fixture.config.to_string_lossy().into_owned(),
        "--resume".to_owned(),
        id,
        "--timeout-seconds".to_owned(),
        "0".to_owned(),
    ]);
    let still_unknown = json_report(&resumed, 2);
    assert_eq!(still_unknown["run"]["state"], "unknown");
    assert_eq!(
        fs::read_to_string(&fixture.dispatch_marker)
            .unwrap()
            .lines()
            .count(),
        1
    );

    let retry = fixture.run_cli(&[
        "--json".to_owned(),
        "run".to_owned(),
        "--config".to_owned(),
        fixture.config.to_string_lossy().into_owned(),
        "--resume".to_owned(),
        reported_request_id(&unknown).to_owned(),
        "--retry".to_owned(),
        "--timeout-seconds".to_owned(),
        "0".to_owned(),
    ]);
    let retry_report = json_report(&retry, 1);
    assert_eq!(error_code(&retry_report), "unknown_retry_not_allowed");
    assert_secret_free(&retry, &[HOOK_SECRET]);
    assert_eq!(
        fs::read_to_string(&fixture.dispatch_marker)
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn local_mode_rejects_server_config_and_redacts_human_and_json_errors() {
    let fixture = Fixture::new();
    fixture.write_config("server", &[("lane-one", "project-one", "dev")], false);
    let server_config = fixture.run_cli(&fixture.run_args(&["--timeout-seconds", "0"]));
    let server_report = json_report(&server_config, 1);
    assert_eq!(error_code(&server_report), "wrong_mode");
    assert_secret_free(
        &server_config,
        &[
            "SHIPBRAID_LOCAL_CLI_PRIVATE_TOKEN_CANARY",
            CONFIG_SECRET,
            HOOK_SECRET,
        ],
    );
    assert!(!fixture.database.exists());

    let malformed = fixture.root.join(format!("{CONFIG_SECRET}.toml"));
    fs::write(&malformed, format!("path = \"{HOOK_SECRET}\n")).unwrap();
    let json_error = fixture.run_cli(&[
        "--json".to_owned(),
        "run".to_owned(),
        "--config".to_owned(),
        malformed.to_string_lossy().into_owned(),
        "--revision".to_owned(),
        fixture.revisions[0].clone(),
        "--components".to_owned(),
        "app".to_owned(),
    ]);
    let json_report = json_report(&json_error, 1);
    assert_eq!(error_code(&json_report), "local_config");
    assert_secret_free(&json_error, &[CONFIG_SECRET, HOOK_SECRET]);

    let human_error = fixture.run_cli(&[
        "run".to_owned(),
        "--config".to_owned(),
        malformed.to_string_lossy().into_owned(),
        "--revision".to_owned(),
        fixture.revisions[0].clone(),
        "--components".to_owned(),
        "app".to_owned(),
    ]);
    assert_eq!(human_error.status.code(), Some(1));
    assert_secret_free(&human_error, &[CONFIG_SECRET, HOOK_SECRET]);
}

#[test]
fn run_arguments_reject_mixed_resume_and_retry_requires_resume() {
    let fixture = Fixture::new();
    let mixed = fixture.run_cli(&[
        "--json".to_owned(),
        "run".to_owned(),
        "--config".to_owned(),
        fixture.config.to_string_lossy().into_owned(),
        "--resume".to_owned(),
        "00000000-0000-4000-8000-000000000001".to_owned(),
        "--revision".to_owned(),
        fixture.revisions[0].clone(),
        "--components".to_owned(),
        "app".to_owned(),
    ]);
    assert_eq!(mixed.status.code(), Some(2));
    let retry_without_resume = fixture.run_cli(&[
        "--json".to_owned(),
        "run".to_owned(),
        "--config".to_owned(),
        fixture.config.to_string_lossy().into_owned(),
        "--retry".to_owned(),
    ]);
    assert_eq!(retry_without_resume.status.code(), Some(2));

    let missing_request = fixture.run_cli(&[
        "--json".to_owned(),
        "run".to_owned(),
        "--config".to_owned(),
        fixture.config.to_string_lossy().into_owned(),
        "--resume".to_owned(),
        "00000000-0000-4000-8000-000000000099".to_owned(),
        "--timeout-seconds".to_owned(),
        "0".to_owned(),
    ]);
    let report = json_report(&missing_request, 1);
    assert_eq!(error_code(&report), "request_not_found");
    assert_secret_free(&missing_request, &[CONFIG_SECRET, HOOK_SECRET]);
}

#[test]
fn invalid_submission_options_are_usage_errors_before_config_access() {
    let fixture = Fixture::new();
    let missing_config = fixture
        .root
        .join(format!("{CONFIG_SECRET}-must-not-be-read.toml"));
    let base = vec![
        "--json".to_owned(),
        "run".to_owned(),
        "--config".to_owned(),
        missing_config.to_string_lossy().into_owned(),
        "--revision".to_owned(),
        fixture.revisions[0].clone(),
        "--components".to_owned(),
        "app".to_owned(),
    ];

    let mut invalid_deadline = base.clone();
    invalid_deadline.extend([
        "--deadline".to_owned(),
        "local-cli-invalid-deadline-canary".to_owned(),
    ]);
    let output = fixture.run_cli(&invalid_deadline);
    let report = json_report(&output, 2);
    assert_eq!(error_code(&report), "invalid_deadline");
    assert_secret_free(
        &output,
        &[
            CONFIG_SECRET,
            "local-cli-invalid-deadline-canary",
            HOOK_SECRET,
        ],
    );

    let mut duplicate_parameter = base.clone();
    duplicate_parameter.extend([
        "--param".to_owned(),
        "region=west".to_owned(),
        "--param".to_owned(),
        "region=east".to_owned(),
    ]);
    let output = fixture.run_cli(&duplicate_parameter);
    let report = json_report(&output, 2);
    assert_eq!(error_code(&report), "invalid_parameter");

    let mut invalid_revision = base;
    let revision_index = invalid_revision
        .iter()
        .position(|argument| argument == "--revision")
        .unwrap();
    invalid_revision[revision_index + 1] = "local-cli-short-revision-canary".to_owned();
    let output = fixture.run_cli(&invalid_revision);
    let report = json_report(&output, 2);
    assert_eq!(error_code(&report), "invalid_revision");
    assert_secret_free(&output, &["local-cli-short-revision-canary", CONFIG_SECRET]);

    assert!(!missing_config.exists());
    assert!(!fixture.database.exists());
}
