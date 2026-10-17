use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::SystemTime,
};

use serde_json::Value;
use tempfile::{Builder, TempDir};

const BINARY: &str = env!("CARGO_BIN_EXE_shipbraid");
const API_TOKEN_ENV: &str = "SHIPBRAID_DOCTOR_ENVNAME_PRIVATE_CANARY";
const WEBHOOK_TOKEN_ENV: &str = "SHIPBRAID_DOCTOR_WEBHOOK_ENVNAME_PRIVATE_CANARY";
const API_TOKEN: &str = "doctor-private-api-token-canary-012345678901234567890123456789";
const WEBHOOK_TOKEN: &str = "doctor-private-webhook-token-canary";
const HOOK_ARGUMENT: &str = "doctor-private-hook-argument-canary";
const PRIVATE_REF: &str = "refs/heads/doctor-private-ref-canary";
const PRIVATE_PATH: &str = "doctor-private-path-canary";

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    config: PathBuf,
    database: PathBuf,
    mirror: PathBuf,
    marker: PathBuf,
    sentinel_hook: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = Builder::new()
            .prefix("shipbraid-doctor-private-path-canary-")
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
        fs::write(source.join("revision"), "fixture\n").unwrap();
        git(
            &[
                "-C",
                source.to_str().unwrap(),
                "-c",
                "user.name=Doctor fixture",
                "-c",
                "user.email=doctor@example.invalid",
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
                "user.name=Doctor fixture",
                "-c",
                "user.email=doctor@example.invalid",
                "commit",
                "-qm",
                "synthetic fixture",
            ],
            &root,
        );
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

        let marker = root.join("doctor-private-hook-executed");
        let sentinel_hook = root.join("sentinel-hook.sh");
        let marker_for_shell = marker.to_string_lossy().replace('\'', "'\\''");
        fs::write(
            &sentinel_hook,
            format!("#!/bin/sh\nprintf executed > '{marker_for_shell}'\n"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&sentinel_hook, fs::Permissions::from_mode(0o700)).unwrap();
        }

        let fixture = Self {
            config: root.join("config.toml"),
            database: root.join("database-parent").join("coordinator.db"),
            mirror,
            marker,
            sentinel_hook,
            _temp: temp,
            root,
        };
        fixture.write_config(
            &fixture.mirror,
            "refs/heads/main",
            &fixture.sentinel_hook,
            API_TOKEN_ENV,
            2,
        );
        fixture
    }

    fn write_config(
        &self,
        repository: &Path,
        release_ref: &str,
        hook_path: &Path,
        api_token_env: &str,
        max_active_runs: usize,
    ) {
        let dashboard = self.root.join(PRIVATE_PATH).join("dashboard");
        fs::create_dir_all(&dashboard).unwrap();
        fs::write(dashboard.join("index.html"), "<html>fixture</html>").unwrap();

        let config = format!(
            "database_path = {}\nlisten = \"127.0.0.1:8787\"\nmax_active_runs = {max_active_runs}\ntick_ms = 100\nscripted_phase_ms = 100\ncancel_timeout_seconds = 2\ncoalesce_ms = 0\ndashboard_dir = {}\n\n[[tokens]]\nid = \"operator\"\ntoken_env = {}\nadmin = true\n\n[[lanes]]\nid = \"fixture-test\"\nproject = \"fixture\"\nenvironment = \"test\"\nrepository_id = \"fixture-repository\"\nrepository_path = {}\nrelease_ref = {}\nprofile_digest = \"doctor-v1\"\ncomponents = [\"app\"]\nallow_preemption = false\n\n[lanes.executor]\nkind = \"hooks\"\n\n[lanes.executor.dispatch]\nkind = \"script\"\npath = {}\ninterpreter = \"/bin/sh\"\nargs = [{}]\ntimeout_ms = 1000\n\n[lanes.executor.observe]\nkind = \"command\"\nprogram = \"/bin/true\"\nargs = []\ntimeout_ms = 1000\n\n[lanes.executor.verify]\nkind = \"webhook\"\nurl = \"http://127.0.0.1:1/doctor-must-not-connect\"\ntoken_env = {}\ntimeout_ms = 1000\n",
            toml_path(&self.database),
            toml_path(&dashboard),
            toml_string(api_token_env),
            toml_path(repository),
            toml_string(release_ref),
            toml_path(hook_path),
            toml_string(HOOK_ARGUMENT),
            toml_string(WEBHOOK_TOKEN_ENV),
        );
        fs::write(&self.config, config).unwrap();
    }

    fn run(&self, arguments: &[String], with_credentials: bool) -> Output {
        self.run_with_tokens(
            arguments,
            with_credentials.then_some(API_TOKEN),
            with_credentials.then_some(WEBHOOK_TOKEN),
        )
    }

    fn run_with_tokens(
        &self,
        arguments: &[String],
        api_token: Option<&str>,
        webhook_token: Option<&str>,
    ) -> Output {
        let mut command = Command::new(BINARY);
        command
            .args(arguments)
            .env_clear()
            .env("PATH", env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        if let Some(api_token) = api_token {
            command.env(API_TOKEN_ENV, api_token);
        }
        if let Some(webhook_token) = webhook_token {
            command.env(WEBHOOK_TOKEN_ENV, webhook_token);
        }
        command.output().unwrap()
    }

    fn doctor_args(&self, json: bool) -> Vec<String> {
        let mut arguments = Vec::new();
        if json {
            arguments.push("--json".to_owned());
        }
        arguments.extend([
            "doctor".to_owned(),
            "--config".to_owned(),
            self.config.to_string_lossy().into_owned(),
        ]);
        arguments
    }
}

fn toml_path(value: &Path) -> String {
    toml_string(value.to_str().unwrap())
}

fn toml_string(value: &str) -> String {
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
        "fixture Git command failed with status {}",
        output.status
    );
}

#[derive(Debug, PartialEq, Eq)]
struct SnapshotEntry {
    modified: SystemTime,
    content: Vec<u8>,
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, SnapshotEntry> {
    fn visit(root: &Path, current: &Path, files: &mut BTreeMap<PathBuf, SnapshotEntry>) {
        for entry in fs::read_dir(current).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let modified = metadata.modified().unwrap();
            if metadata.is_dir() {
                files.insert(
                    relative.clone(),
                    SnapshotEntry {
                        modified,
                        content: Vec::new(),
                    },
                );
                visit(root, &path, files);
            } else if metadata.is_file() {
                files.insert(
                    relative,
                    SnapshotEntry {
                        modified,
                        content: fs::read(&path).unwrap(),
                    },
                );
            } else {
                files.insert(
                    relative,
                    SnapshotEntry {
                        modified,
                        content: Vec::new(),
                    },
                );
            }
        }
    }

    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

fn report(output: &Output, expected_code: i32) -> Value {
    assert_eq!(output.status.code(), Some(expected_code));
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "doctor did not return a JSON report: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn assert_secret_free(output: &Output, secrets: &[&str]) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for secret in secrets {
        assert!(!text.contains(secret), "doctor output disclosed {secret:?}");
    }
}

fn check_status<'a>(report: &'a Value, code: &str, field: &str) -> &'a str {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["code"] == code && check["field"] == field)
        .unwrap_or_else(|| panic!("missing {code} check for {field}: {report}"))["status"]
        .as_str()
        .unwrap()
}

#[test]
fn doctor_reports_success_in_json_and_text_without_side_effects() {
    let fixture = Fixture::new();
    let config_before = fs::read(&fixture.config).unwrap();
    let mirror_before = snapshot_tree(&fixture.mirror);

    let output = fixture.run(&fixture.doctor_args(true), true);
    let value = report(&output, 0);
    assert_eq!(value["schema_version"], 1);
    assert!(value["version"].as_str().is_some());
    assert!(value["ok"].as_bool().unwrap());
    assert!(value["scope"].as_str().is_some());
    assert_eq!(check_status(&value, "config.validate", "config"), "pass");
    assert_eq!(
        check_status(&value, "repository.release_ref", "lanes[0].release_ref"),
        "pass"
    );
    assert_eq!(
        check_status(&value, "config.api_token", "tokens[0].token_env"),
        "pass"
    );
    assert_eq!(
        check_status(
            &value,
            "config.webhook_credential",
            "lanes[0].executor.verify.token_env"
        ),
        "pass"
    );
    assert!(value["summary"]["skipped"].as_u64().unwrap() > 0);
    assert_eq!(
        check_status(&value, "runtime.user_systemd", "runtime.user_systemd"),
        "skipped"
    );
    assert_eq!(
        check_status(&value, "runtime.cgroup_v2", "runtime.cgroup_v2"),
        "skipped"
    );
    assert_secret_free(
        &output,
        &[
            API_TOKEN_ENV,
            WEBHOOK_TOKEN_ENV,
            API_TOKEN,
            WEBHOOK_TOKEN,
            HOOK_ARGUMENT,
            PRIVATE_PATH,
        ],
    );

    let text_output = fixture.run(&fixture.doctor_args(false), true);
    assert!(text_output.status.success());
    let text = String::from_utf8_lossy(&text_output.stdout);
    assert!(text.contains("Shipbraid doctor"));
    assert!(text.contains("Summary:"));
    assert_secret_free(
        &text_output,
        &[
            API_TOKEN_ENV,
            WEBHOOK_TOKEN_ENV,
            API_TOKEN,
            WEBHOOK_TOKEN,
            HOOK_ARGUMENT,
            PRIVATE_PATH,
        ],
    );

    assert_eq!(fs::read(&fixture.config).unwrap(), config_before);
    assert_eq!(snapshot_tree(&fixture.mirror), mirror_before);
    assert!(
        !fixture.marker.exists(),
        "doctor executed the sentinel hook"
    );
    assert!(!fixture.database.parent().unwrap().exists());
    for suffix in ["", "-wal", "-shm", "-journal", ".lock"] {
        let mut path = OsString::from(fixture.database.as_os_str());
        path.push(suffix);
        assert!(
            !PathBuf::from(path).exists(),
            "doctor created a ledger artifact"
        );
    }
}

#[test]
fn doctor_runs_before_api_url_and_token_file_handling() {
    let fixture = Fixture::new();
    let mut arguments = vec![
        "--json".to_owned(),
        "--url".to_owned(),
        "file:///doctor-private-invalid-url-canary".to_owned(),
        "--token-file".to_owned(),
        fixture
            .root
            .join("doctor-private-missing-token-file-canary")
            .to_string_lossy()
            .into_owned(),
    ];
    arguments.extend(fixture.doctor_args(false));
    let output = fixture.run(&arguments, true);
    let value = report(&output, 0);
    assert!(value["ok"].as_bool().unwrap());
    assert_secret_free(
        &output,
        &[
            "doctor-private-invalid-url-canary",
            "doctor-private-missing-token-file-canary",
            API_TOKEN,
            API_TOKEN_ENV,
        ],
    );
}

#[test]
fn doctor_never_executes_configured_programs_or_path_supplied_probes() {
    let fixture = Fixture::new();
    let config = fs::read_to_string(&fixture.config)
        .unwrap()
        .replace(
            "interpreter = \"/bin/sh\"",
            &format!("interpreter = {}", toml_path(&fixture.sentinel_hook)),
        )
        .replace(
            "program = \"/bin/true\"",
            &format!("program = {}", toml_path(&fixture.sentinel_hook)),
        );
    fs::write(&fixture.config, config).unwrap();
    let fake_bin = fixture.root.join("bin");
    fs::create_dir(&fake_bin).unwrap();
    for tool in ["git", "systemctl"] {
        fs::copy(&fixture.sentinel_hook, fake_bin.join(tool)).unwrap();
    }
    let before = snapshot_tree(&fixture.root);
    let output = Command::new(BINARY)
        .args(fixture.doctor_args(true))
        .env_clear()
        .env("PATH", &fake_bin)
        .env(API_TOKEN_ENV, API_TOKEN)
        .env(WEBHOOK_TOKEN_ENV, WEBHOOK_TOKEN)
        .env("GIT_DIR", &fake_bin)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.bare")
        .env("GIT_CONFIG_VALUE_0", "false")
        .output()
        .unwrap();
    assert_eq!(report(&output, 0)["ok"], true);
    assert_eq!(snapshot_tree(&fixture.root), before);
    assert!(!fixture.marker.exists());
    assert_secret_free(
        &output,
        &[API_TOKEN, WEBHOOK_TOKEN, HOOK_ARGUMENT, PRIVATE_PATH],
    );
}

#[test]
fn doctor_returns_one_for_bad_configuration_and_redacts_parser_and_values() {
    let fixture = Fixture::new();
    let malformed_canary = "doctor-private-malformed-toml-canary";
    fs::write(
        &fixture.config,
        format!("database_path = \"{malformed_canary}\n"),
    )
    .unwrap();
    let malformed = fixture.run(&fixture.doctor_args(true), true);
    let malformed_report = report(&malformed, 1);
    assert!(!malformed_report["ok"].as_bool().unwrap());
    assert_eq!(
        check_status(&malformed_report, "config.load", "config"),
        "fail"
    );
    assert_secret_free(&malformed, &[malformed_canary, PRIVATE_PATH, API_TOKEN]);

    fixture.write_config(
        &fixture.mirror,
        "refs/heads/main",
        &fixture.sentinel_hook,
        API_TOKEN_ENV,
        0,
    );
    let invalid = fixture.run(&fixture.doctor_args(true), true);
    let invalid_report = report(&invalid, 1);
    assert!(!invalid_report["ok"].as_bool().unwrap());
    assert_eq!(
        check_status(&invalid_report, "config.validate", "config"),
        "fail"
    );
    assert_secret_free(
        &invalid,
        &[
            API_TOKEN_ENV,
            WEBHOOK_TOKEN_ENV,
            API_TOKEN,
            WEBHOOK_TOKEN,
            HOOK_ARGUMENT,
            PRIVATE_PATH,
        ],
    );
}

#[test]
fn doctor_reports_missing_credentials_bad_mirror_ref_and_hook_path() {
    let fixture = Fixture::new();
    fixture.write_config(
        &fixture.mirror,
        "refs/heads/main",
        &fixture.sentinel_hook,
        API_TOKEN_ENV,
        2,
    );
    let missing_credentials = fixture.run(&fixture.doctor_args(true), false);
    let missing_report = report(&missing_credentials, 1);
    assert_eq!(
        check_status(&missing_report, "config.api_token", "tokens[0].token_env"),
        "fail"
    );
    assert_eq!(
        check_status(
            &missing_report,
            "config.webhook_credential",
            "lanes[0].executor.verify.token_env"
        ),
        "fail"
    );
    assert_secret_free(&missing_credentials, &[API_TOKEN_ENV, WEBHOOK_TOKEN_ENV]);

    let non_bare = fixture.root.join("doctor-private-non-bare-mirror-canary");
    fs::create_dir_all(&non_bare).unwrap();
    fixture.write_config(
        &non_bare,
        "refs/heads/main",
        &fixture.sentinel_hook,
        API_TOKEN_ENV,
        2,
    );
    let bad_mirror = fixture.run(&fixture.doctor_args(true), true);
    let bad_mirror_report = report(&bad_mirror, 1);
    assert_eq!(
        check_status(
            &bad_mirror_report,
            "repository.bare",
            "lanes[0].repository_path"
        ),
        "fail"
    );
    assert_secret_free(
        &bad_mirror,
        &["doctor-private-non-bare-mirror-canary", API_TOKEN],
    );

    fixture.write_config(
        &fixture.mirror,
        PRIVATE_REF,
        &fixture
            .root
            .join("doctor-private-missing-hook-path-canary.sh"),
        API_TOKEN_ENV,
        2,
    );
    let bad_ref_and_hook = fixture.run(&fixture.doctor_args(true), true);
    let bad_ref_and_hook_report = report(&bad_ref_and_hook, 1);
    assert_eq!(
        check_status(
            &bad_ref_and_hook_report,
            "repository.release_ref",
            "lanes[0].release_ref"
        ),
        "fail"
    );
    assert_eq!(
        check_status(
            &bad_ref_and_hook_report,
            "config.hook_script",
            "lanes[0].executor.dispatch.path"
        ),
        "fail"
    );
    assert_secret_free(
        &bad_ref_and_hook,
        &[
            PRIVATE_REF,
            "doctor-private-missing-hook-path-canary",
            API_TOKEN_ENV,
            API_TOKEN,
            HOOK_ARGUMENT,
        ],
    );
    assert!(
        !fixture.marker.exists(),
        "doctor executed the sentinel hook"
    );
}

#[test]
fn doctor_redacts_invalid_api_token_and_environment_reference() {
    let fixture = Fixture::new();
    fixture.write_config(
        &fixture.mirror,
        "refs/heads/main",
        &fixture.sentinel_hook,
        API_TOKEN_ENV,
        2,
    );

    let short_token = "doctor-short-token-canary";
    let short = fixture.run_with_tokens(
        &fixture.doctor_args(true),
        Some(short_token),
        Some(WEBHOOK_TOKEN),
    );
    let short_report = report(&short, 1);
    assert_eq!(
        check_status(&short_report, "config.api_token", "tokens[0].token_env"),
        "fail"
    );
    assert_secret_free(
        &short,
        &[short_token, API_TOKEN_ENV, API_TOKEN, WEBHOOK_TOKEN],
    );

    let invalid_env_reference = "doctor-private-invalid-env-reference-canary=";
    fixture.write_config(
        &fixture.mirror,
        "refs/heads/main",
        &fixture.sentinel_hook,
        invalid_env_reference,
        2,
    );
    let invalid_env = fixture.run(&fixture.doctor_args(true), true);
    let invalid_env_report = report(&invalid_env, 1);
    assert_eq!(
        check_status(
            &invalid_env_report,
            "config.api_token",
            "tokens[0].token_env"
        ),
        "fail"
    );
    assert_secret_free(
        &invalid_env,
        &[
            invalid_env_reference,
            API_TOKEN_ENV,
            WEBHOOK_TOKEN_ENV,
            API_TOKEN,
            WEBHOOK_TOKEN,
        ],
    );
}

#[test]
fn doctor_preserves_an_existing_invalid_database_without_lock_artifacts() {
    let fixture = Fixture::new();
    let database_parent = fixture.database.parent().unwrap();
    fs::create_dir_all(database_parent).unwrap();
    let invalid_database = b"doctor-private-invalid-sqlite-file-canary\n";
    fs::write(&fixture.database, invalid_database).unwrap();
    let database_before = snapshot_tree(database_parent);

    let output = fixture.run(&fixture.doctor_args(true), true);
    let value = report(&output, 0);
    assert!(value["ok"].as_bool().unwrap());
    assert_eq!(snapshot_tree(database_parent), database_before);
    assert_eq!(fs::read(&fixture.database).unwrap(), invalid_database);
    for suffix in ["-wal", "-shm", "-journal", ".lock"] {
        let mut path = OsString::from(fixture.database.as_os_str());
        path.push(suffix);
        assert!(
            !PathBuf::from(path).exists(),
            "doctor created a ledger sidecar or lock"
        );
    }
    assert!(
        !fixture.marker.exists(),
        "doctor executed the sentinel hook"
    );
    assert_secret_free(
        &output,
        &[PRIVATE_PATH, API_TOKEN, "invalid-sqlite-file-canary"],
    );
}

#[test]
fn doctor_usage_error_returns_two() {
    let fixture = Fixture::new();
    let output = fixture.run(&["--json".to_owned(), "doctor".to_owned()], false);
    assert_eq!(output.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["code"], "invalid_arguments");
    assert_secret_free(&output, &[API_TOKEN_ENV, API_TOKEN, PRIVATE_PATH]);
}
