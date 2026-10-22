use std::{
    collections::HashSet,
    env::VarError,
    ffi::OsStr,
    fs::{self, File},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::config::{Config, HookConfig, LaneExecutorConfig};

use super::{Check, CheckStatus};

pub(super) fn configuration_checks(config: &Config) -> Vec<Check> {
    configuration_checks_with(
        config,
        &|name| std::env::var(name),
        std::env::var_os("PATH").as_deref(),
    )
}

fn configuration_checks_with<F>(
    config: &Config,
    read_env: &F,
    search_path: Option<&OsStr>,
) -> Vec<Check>
where
    F: Fn(&str) -> Result<String, VarError>,
{
    let mut checks = Vec::new();
    api_token_checks(config, read_env, &mut checks);

    for (lane_index, lane) in config.lanes.iter().enumerate() {
        let lane_field = format!("lanes[{lane_index}]");
        match &lane.executor {
            LaneExecutorConfig::Scripted => {}
            LaneExecutorConfig::Hooks {
                dispatch,
                observe,
                verify,
            } => {
                for (operation, hook) in [
                    ("dispatch", dispatch),
                    ("observe", observe),
                    ("verify", verify),
                ] {
                    hook_checks(
                        hook,
                        &format!("{lane_field}.executor.{operation}"),
                        read_env,
                        search_path,
                        &mut checks,
                    );
                }
            }
        }
        if let Some(hook) = &lane.failure_observe {
            hook_checks(
                hook,
                &format!("{lane_field}.failure_observe"),
                read_env,
                search_path,
                &mut checks,
            );
        }
    }

    if let Some(dashboard_dir) = &config.dashboard_dir {
        let index = dashboard_dir.join("index.html");
        let (status, message, suggestion) = if readable_regular_file(&index) {
            (
                CheckStatus::Pass,
                "Configured dashboard index is present and readable.",
                "No action is needed.",
            )
        } else {
            (
                CheckStatus::Fail,
                "Configured dashboard index is missing or unreadable.",
                "Install a readable index.html in the configured dashboard directory.",
            )
        };
        checks.push(Check::new(
            "config.dashboard_index",
            status,
            "dashboard_dir/index.html",
            message,
            suggestion,
        ));
    }

    if let Some(artifacts_dir) = &config.artifacts_dir {
        let (status, message, suggestion) = if artifacts_dir.is_dir() {
            (
                CheckStatus::Pass,
                "Configured artifacts directory is present.",
                "No action is needed.",
            )
        } else {
            (
                CheckStatus::Fail,
                "Configured artifacts directory does not exist or is not a directory.",
                "Create a readable artifacts directory.",
            )
        };
        checks.push(Check::new(
            "config.artifacts_dir",
            status,
            "artifacts_dir",
            message,
            suggestion,
        ));
    }

    checks
}

fn api_token_checks<F>(config: &Config, read_env: &F, checks: &mut Vec<Check>)
where
    F: Fn(&str) -> Result<String, VarError>,
{
    let mut seen = HashSet::new();
    for (index, token) in config.tokens.iter().enumerate() {
        let field = format!("tokens[{index}].token_env");
        let check = match read_environment_value(&token.token_env, read_env) {
            Err(EnvProblem::InvalidReference | EnvProblem::Missing) => Check::new(
                "config.api_token",
                CheckStatus::Fail,
                field,
                "Referenced API token is unavailable.",
                "Set the configured API token environment reference to a UTF-8 value of at least 32 bytes.",
            ),
            Err(EnvProblem::NotUnicode) => Check::new(
                "config.api_token",
                CheckStatus::Fail,
                field,
                "Referenced API token is not valid UTF-8.",
                "Set the configured API token environment reference to a UTF-8 value of at least 32 bytes.",
            ),
            Ok(secret) if secret.len() < 32 => Check::new(
                "config.api_token",
                CheckStatus::Fail,
                field,
                "API token does not meet the runtime minimum length.",
                "Set the configured API token environment reference to a UTF-8 value of at least 32 bytes.",
            ),
            Ok(secret) => {
                let fingerprint: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
                if seen.insert(fingerprint) {
                    Check::new(
                        "config.api_token",
                        CheckStatus::Pass,
                        field,
                        "Referenced API token is available and meets runtime requirements.",
                        "No action is needed.",
                    )
                } else {
                    Check::new(
                        "config.api_token",
                        CheckStatus::Fail,
                        field,
                        "API token duplicates another configured credential.",
                        "Use a distinct value for each configured API token.",
                    )
                }
            }
        };
        checks.push(check);
    }
}

#[derive(Clone, Copy)]
enum EnvProblem {
    InvalidReference,
    Missing,
    NotUnicode,
}

fn read_environment_value<F>(name: &str, read_env: &F) -> Result<String, EnvProblem>
where
    F: Fn(&str) -> Result<String, VarError>,
{
    // Avoid passing invalid process-environment keys to the platform API.
    if name.is_empty() || name.contains('=') || name.contains('\0') {
        return Err(EnvProblem::InvalidReference);
    }
    read_env(name).map_err(|error| match error {
        VarError::NotPresent => EnvProblem::Missing,
        VarError::NotUnicode(_) => EnvProblem::NotUnicode,
    })
}

fn hook_checks<F>(
    hook: &HookConfig,
    field: &str,
    read_env: &F,
    search_path: Option<&OsStr>,
    checks: &mut Vec<Check>,
) where
    F: Fn(&str) -> Result<String, VarError>,
{
    match hook {
        HookConfig::Command { program, cwd, .. } => {
            executable_check(
                program,
                &format!("{field}.program"),
                search_path,
                cwd.as_deref(),
                checks,
            );
            if let Some(cwd) = cwd {
                cwd_check(cwd, &format!("{field}.cwd"), checks);
            }
        }
        HookConfig::Script {
            path,
            interpreter,
            cwd,
            ..
        } => {
            let effective_cwd = cwd.as_deref().or_else(|| path.parent());
            executable_check(
                interpreter,
                &format!("{field}.interpreter"),
                search_path,
                effective_cwd,
                checks,
            );
            let (status, message, suggestion) = if readable_regular_file(path) {
                (
                    CheckStatus::Pass,
                    "Configured hook script is a readable regular file.",
                    "No action is needed.",
                )
            } else {
                (
                    CheckStatus::Fail,
                    "Configured hook script is missing or unreadable.",
                    "Provide a readable regular file at the configured script path.",
                )
            };
            checks.push(Check::new(
                "config.hook_script",
                status,
                format!("{field}.path"),
                message,
                suggestion,
            ));
            if let Some(cwd) = cwd {
                cwd_check(cwd, &format!("{field}.cwd"), checks);
            }
        }
        HookConfig::Webhook { token_env, .. } => {
            let credential_field = format!("{field}.token_env");
            let check = match read_environment_value(token_env, read_env) {
                Err(EnvProblem::InvalidReference | EnvProblem::Missing) => Check::new(
                    "config.webhook_credential",
                    CheckStatus::Fail,
                    credential_field,
                    "Referenced webhook credential is unavailable.",
                    "Set the configured webhook credential reference to a nonempty UTF-8 value accepted by an HTTP bearer header.",
                ),
                Err(EnvProblem::NotUnicode) => Check::new(
                    "config.webhook_credential",
                    CheckStatus::Fail,
                    credential_field,
                    "Referenced webhook credential is not valid UTF-8.",
                    "Set the configured webhook credential reference to a nonempty UTF-8 value accepted by an HTTP bearer header.",
                ),
                Ok(value) if value.is_empty() || value.len() > 8192 => Check::new(
                    "config.webhook_credential",
                    CheckStatus::Fail,
                    credential_field,
                    "Webhook credential does not meet runtime size requirements.",
                    "Set the configured webhook credential reference to a nonempty value of at most 8192 bytes accepted by an HTTP bearer header.",
                ),
                Ok(value) if !valid_bearer_header(&value) => Check::new(
                    "config.webhook_credential",
                    CheckStatus::Fail,
                    credential_field,
                    "Webhook credential cannot be represented as a bearer header.",
                    "Set the configured webhook credential reference to a nonempty value of at most 8192 bytes accepted by an HTTP bearer header.",
                ),
                Ok(_) => Check::new(
                    "config.webhook_credential",
                    CheckStatus::Pass,
                    credential_field,
                    "Referenced webhook credential meets runtime requirements.",
                    "No action is needed.",
                ),
            };
            checks.push(check);
        }
    }
}

fn valid_bearer_header(token: &str) -> bool {
    reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).is_ok()
}

fn executable_check(
    program: &Path,
    field: &str,
    search_path: Option<&OsStr>,
    cwd: Option<&Path>,
    checks: &mut Vec<Check>,
) {
    let available = if program.as_os_str().is_empty() {
        false
    } else if program.is_absolute()
        || program.components().count() > 1
        || program
            .to_string_lossy()
            .contains(std::path::MAIN_SEPARATOR)
    {
        is_executable_file(&resolve_against_cwd(program, cwd))
    } else {
        find_executable(program, search_path, cwd)
    };
    let (status, message, suggestion) = if available {
        (
            CheckStatus::Pass,
            "Configured executable is present and executable.",
            "No action is needed.",
        )
    } else {
        (
            CheckStatus::Fail,
            "Configured executable is missing or not executable.",
            "Install an executable at the configured path or make it available through PATH.",
        )
    };
    checks.push(Check::new(
        "config.hook_executable",
        status,
        field.to_owned(),
        message,
        suggestion,
    ));
}

fn find_executable(program: &Path, search_path: Option<&OsStr>, cwd: Option<&Path>) -> bool {
    // Linux execvp uses the system default search path when PATH is absent.
    let default_search_path = {
        #[cfg(target_os = "linux")]
        {
            Some(OsStr::new("/bin:/usr/bin"))
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    };
    let search_path = search_path.or(default_search_path);
    search_path.is_some_and(|search_path| {
        std::env::split_paths(search_path).any(|directory| {
            let directory = if directory.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                directory
            };
            is_executable_file(&resolve_against_cwd(&directory, cwd).join(program))
        })
    })
}

fn resolve_against_cwd(path: &Path, cwd: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.unwrap_or_else(|| Path::new(".")).join(path)
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn cwd_check(cwd: &Path, field: &str, checks: &mut Vec<Check>) {
    let readable_directory =
        fs::metadata(cwd).is_ok_and(|metadata| metadata.is_dir() && fs::read_dir(cwd).is_ok());
    let (status, message, suggestion) = if readable_directory {
        (
            CheckStatus::Pass,
            "Configured hook working directory is present and readable.",
            "No action is needed.",
        )
    } else {
        (
            CheckStatus::Fail,
            "Configured hook working directory is missing or unreadable.",
            "Provide a readable directory at the configured working directory path.",
        )
    };
    checks.push(Check::new(
        "config.hook_cwd",
        status,
        field.to_owned(),
        message,
        suggestion,
    ));
}

fn readable_regular_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) && File::open(path).is_ok()
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, ffi::OsString, fs, path::PathBuf};

    use super::*;
    use crate::config::{HookConfig, LaneConfig, TokenConfig};

    fn env_map(values: &[(&str, &str)]) -> HashMap<String, String> {
        values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn lane_with(executor: LaneExecutorConfig, failure_observe: Option<HookConfig>) -> LaneConfig {
        LaneConfig {
            id: "internal-test-id".into(),
            project: "project".into(),
            environment: "test".into(),
            repository_id: "repo".into(),
            repository_path: PathBuf::from("."),
            release_ref: "HEAD".into(),
            profile_digest: "profile".into(),
            components: vec!["app".into()],
            request_parameters: Default::default(),
            allow_descendant: false,
            allow_preemption: false,
            enabled: true,
            executor,
            failure_observe,
            resource_admission: None,
        }
    }

    fn token(env: &str) -> TokenConfig {
        TokenConfig {
            id: "internal-test-token-id".into(),
            token_env: env.into(),
            lanes: vec![],
            projects: vec!["project".into()],
            urgent: false,
            admin: false,
            executor: false,
        }
    }

    fn status_for(checks: &[Check], field: &str) -> CheckStatus {
        checks
            .iter()
            .find(|check| check.field == field)
            .unwrap()
            .status
    }

    #[test]
    fn api_tokens_follow_runtime_utf8_length_and_uniqueness_rules_without_disclosing_values() {
        let config = Config {
            tokens: vec![
                token("TOKEN_A"),
                token("TOKEN_B"),
                token("TOKEN_MISSING"),
                token("TOKEN_SHORT"),
                token("TOKEN_NON_UNICODE"),
            ],
            ..Config::default()
        };
        let values = env_map(&[
            ("TOKEN_A", "a sufficiently long token value 0001"),
            ("TOKEN_B", "a sufficiently long token value 0001"),
            ("TOKEN_SHORT", "short"),
        ]);
        let checks = configuration_checks_with(
            &config,
            &|name| match name {
                "TOKEN_NON_UNICODE" => Err(VarError::NotUnicode(OsString::from("secret-canary"))),
                _ => values.get(name).cloned().ok_or(VarError::NotPresent),
            },
            None,
        );

        assert_eq!(
            status_for(&checks, "tokens[0].token_env"),
            CheckStatus::Pass
        );
        assert_eq!(
            status_for(&checks, "tokens[1].token_env"),
            CheckStatus::Fail
        );
        assert_eq!(
            status_for(&checks, "tokens[2].token_env"),
            CheckStatus::Fail
        );
        assert_eq!(
            status_for(&checks, "tokens[3].token_env"),
            CheckStatus::Fail
        );
        assert_eq!(
            status_for(&checks, "tokens[4].token_env"),
            CheckStatus::Fail
        );
        let report = checks
            .iter()
            .map(|check| format!("{} {} {}", check.field, check.message, check.suggestion))
            .collect::<String>();
        assert!(!report.contains("sufficiently long"));
        assert!(!report.contains("TOKEN_A"));
        assert!(!report.contains("secret-canary"));
    }

    #[test]
    fn hook_paths_check_executables_scripts_working_directories_and_dashboard_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        let work = temp.path().join("work");
        let dashboard = temp.path().join("dashboard");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&dashboard).unwrap();
        let command = bin.join("hook-command");
        let interpreter = bin.join("python");
        let script = temp.path().join("hook.py");
        fs::write(&command, "fixture").unwrap();
        fs::write(&interpreter, "fixture").unwrap();
        fs::write(&script, "print('fixture')").unwrap();
        fs::write(dashboard.join("index.html"), "fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&command, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o755)).unwrap();
            // Interpreted scripts need to be readable, not directly executable.
            fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(!is_executable_file(&script));
        }

        let command_hook = HookConfig::Command {
            program: PathBuf::from("hook-command"),
            args: vec![],
            cwd: Some(work.clone()),
            timeout_ms: 1000,
        };
        let script_hook = HookConfig::Script {
            path: script,
            interpreter: PathBuf::from("python"),
            args: vec![],
            cwd: Some(work),
            timeout_ms: 1000,
        };
        let config = Config {
            lanes: vec![lane_with(
                LaneExecutorConfig::Hooks {
                    dispatch: command_hook.clone(),
                    observe: script_hook.clone(),
                    verify: command_hook,
                },
                Some(script_hook),
            )],
            dashboard_dir: Some(dashboard),
            ..Config::default()
        };
        let checks = configuration_checks_with(
            &config,
            &|_| Err(VarError::NotPresent),
            Some(bin.as_os_str()),
        );

        assert_eq!(
            status_for(&checks, "lanes[0].executor.dispatch.program"),
            CheckStatus::Pass
        );
        assert_eq!(
            status_for(&checks, "lanes[0].executor.observe.interpreter"),
            CheckStatus::Pass
        );
        assert_eq!(
            status_for(&checks, "lanes[0].executor.observe.path"),
            CheckStatus::Pass
        );
        assert_eq!(
            status_for(&checks, "lanes[0].failure_observe.path"),
            CheckStatus::Pass
        );
        assert_eq!(
            status_for(&checks, "lanes[0].executor.dispatch.cwd"),
            CheckStatus::Pass
        );
        assert_eq!(
            status_for(&checks, "dashboard_dir/index.html"),
            CheckStatus::Pass
        );
    }

    #[test]
    fn relative_path_entries_are_resolved_from_the_hook_working_directory() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("hook-cwd");
        let bin = cwd.join("relative-bin");
        fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("doctor-hook-executable-fixture");
        fs::write(&executable, "fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        }

        assert!(find_executable(
            Path::new("doctor-hook-executable-fixture"),
            Some(OsStr::new("relative-bin")),
            Some(&cwd),
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_path_uses_the_unix_execvp_default_search_directories() {
        assert!(find_executable(Path::new("sh"), None, None));
    }

    #[test]
    fn webhook_credentials_match_bearer_header_and_size_constraints() {
        let config = Config {
            lanes: vec![lane_with(
                LaneExecutorConfig::Hooks {
                    dispatch: HookConfig::Webhook {
                        url: "https://example.test/dispatch".into(),
                        token_env: "WEBHOOK_GOOD".into(),
                        timeout_ms: 1000,
                    },
                    observe: HookConfig::Webhook {
                        url: "https://example.test/observe".into(),
                        token_env: "WEBHOOK_BAD".into(),
                        timeout_ms: 1000,
                    },
                    verify: HookConfig::Webhook {
                        url: "https://example.test/verify".into(),
                        token_env: "WEBHOOK_LARGE".into(),
                        timeout_ms: 1000,
                    },
                },
                None,
            )],
            ..Config::default()
        };
        let values = env_map(&[
            ("WEBHOOK_GOOD", "valid-token"),
            ("WEBHOOK_BAD", "bad\nheader"),
            ("WEBHOOK_LARGE", &"x".repeat(8193)),
        ]);
        let checks = configuration_checks_with(
            &config,
            &|name| values.get(name).cloned().ok_or(VarError::NotPresent),
            None,
        );

        assert_eq!(
            status_for(&checks, "lanes[0].executor.dispatch.token_env"),
            CheckStatus::Pass
        );
        assert_eq!(
            status_for(&checks, "lanes[0].executor.observe.token_env"),
            CheckStatus::Fail
        );
        assert_eq!(
            status_for(&checks, "lanes[0].executor.verify.token_env"),
            CheckStatus::Fail
        );
        let report = checks
            .iter()
            .map(|check| format!("{} {} {}", check.field, check.message, check.suggestion))
            .collect::<String>();
        assert!(!report.contains("bad\nheader"));
        assert!(!report.contains("valid-token"));
    }
}
