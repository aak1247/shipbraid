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
        if let Some(guide) = &lane.guide {
            guide_checks(guide, &lane_field, &mut checks);
        }
        hook_environment_and_credential_checks(lane, &lane_field, &mut checks);
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

fn guide_checks(guide: &crate::config::GuideConfig, lane_field: &str, checks: &mut Vec<Check>) {
    if let Some(project) = &guide.project {
        check_guide_file(project, &format!("{lane_field}.guide.project"), checks);
    }
    if let Some(environment) = &guide.environment {
        check_guide_file(
            environment,
            &format!("{lane_field}.guide.environment"),
            checks,
        );
    }
    for (component, path) in &guide.components {
        check_guide_file(
            path,
            &format!("{lane_field}.guide.components.{component}"),
            checks,
        );
    }
}

fn check_guide_file(path: &Path, field: &str, checks: &mut Vec<Check>) {
    if !path.is_file() {
        checks.push(Check::new(
            "config.guide_file",
            CheckStatus::Fail,
            field,
            "Configured guide file is missing or not a regular file.",
            "Create a readable markdown file at the configured path.",
        ));
        return;
    }
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => {
            checks.push(Check::new(
                "config.guide_file",
                CheckStatus::Fail,
                field,
                "Configured guide file metadata cannot be read.",
                "Ensure the configured guide file is accessible.",
            ));
            return;
        }
    };
    if metadata.len() > crate::config::MAX_GUIDE_FILE_BYTES as u64 {
        checks.push(Check::new(
            "config.guide_file",
            CheckStatus::Fail,
            field,
            "Configured guide file exceeds maximum allowed size.",
            "Reduce the guide file size to 32768 bytes or less.",
        ));
        return;
    }
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => {
            checks.push(Check::new(
                "config.guide_file",
                CheckStatus::Fail,
                field,
                "Configured guide file is not valid UTF-8.",
                "Ensure the guide file is encoded as UTF-8.",
            ));
            return;
        }
    };
    if crate::config::scan_secret(&content).is_some() {
        checks.push(Check::new(
            "config.guide_file",
            CheckStatus::Fail,
            field,
            "Configured guide file contains prohibited secret patterns.",
            "Remove all secrets or credentials from the guide file.",
        ));
        return;
    }
    checks.push(Check::new(
        "config.guide_file",
        CheckStatus::Pass,
        field,
        "Configured guide file is present, within size limits, and contains no secret patterns.",
        "No action is needed.",
    ));
}

fn hook_environment_and_credential_checks(
    lane: &crate::config::LaneConfig,
    lane_field: &str,
    checks: &mut Vec<Check>,
) {
    if lane.executor.kind() != "hooks" {
        if lane.credentials.is_some() {
            checks.push(Check::new(
                "config.credentials_file",
                CheckStatus::Fail,
                format!("{lane_field}.credentials"),
                "Credentials cannot be configured for scripted lanes.",
                "Remove [lanes.credentials] or change executor to 'hooks'.",
            ));
        }
        return;
    }

    let allow_set: HashSet<&str> = lane
        .hook_environment
        .as_ref()
        .map(|e| e.allow.iter().map(|s| s.as_str()).collect())
        .unwrap_or_default();

    if let Some(credentials) = &lane.credentials {
        if credentials.env_files.is_empty() {
            checks.push(Check::new(
                "config.credentials_file",
                CheckStatus::Pass,
                format!("{lane_field}.credentials.env_files"),
                "No credential files configured.",
                "No action is needed.",
            ));
        } else {
            let mut all_keys = HashSet::new();
            let mut total_keys_overflow = false;
            for (file_idx, path) in credentials.env_files.iter().enumerate() {
                let field = format!("{lane_field}.credentials.env_files[{file_idx}]");
                check_credential_file(
                    path,
                    &field,
                    &allow_set,
                    &mut all_keys,
                    &mut total_keys_overflow,
                    checks,
                );
            }
            if total_keys_overflow || all_keys.len() > crate::config::MAX_TOTAL_CREDENTIAL_KEYS {
                checks.push(Check::new(
                    "config.credentials_file",
                    CheckStatus::Fail,
                    format!("{lane_field}.credentials"),
                    "Total credential keys across all files exceeds maximum limit.",
                    "Reduce the total number of credential keys across all files to 32 or fewer.",
                ));
            }
        }
    }

    let mode = lane
        .hook_environment
        .as_ref()
        .map(|e| e.mode)
        .unwrap_or(crate::config::HookEnvironmentMode::Inherit);

    match mode {
        crate::config::HookEnvironmentMode::Isolated => {
            let mut has_invalid_name = false;
            let mut seen_allow = HashSet::new();
            if let Some(env_config) = &lane.hook_environment {
                for name in &env_config.allow {
                    if !crate::config::is_valid_env_key(name)
                        || name.starts_with("SHIPBRAID_")
                        || !seen_allow.insert(name.as_str())
                    {
                        has_invalid_name = true;
                        break;
                    }
                }
            }
            if has_invalid_name {
                checks.push(Check::new(
                    "config.hook_environment",
                    CheckStatus::Fail,
                    format!("{lane_field}.hook_environment.allow"),
                    "Hook environment allowlist contains invalid, reserved, or duplicate variable names.",
                    "Ensure allowlist entries are valid environment variable names without 'SHIPBRAID_' prefix.",
                ));
            } else {
                checks.push(Check::new(
                    "config.hook_environment",
                    CheckStatus::Pass,
                    format!("{lane_field}.hook_environment"),
                    "Configured hook environment is isolated with an allowlist.",
                    "No action is needed.",
                ));
            }
        }
        crate::config::HookEnvironmentMode::Inherit => {
            checks.push(Check::new(
                "config.hook_environment",
                CheckStatus::Warn,
                format!("{lane_field}.hook_environment"),
                "Hook environment uses 'inherit' mode: hook subprocesses inherit coordinator process environment, which may leak ambient credentials.",
                "Configure [lanes.hook_environment] with mode = 'isolated' and an explicit allowlist.",
            ));
        }
    }
}

fn check_credential_file(
    path: &Path,
    field: &str,
    allow_set: &HashSet<&str>,
    all_keys: &mut HashSet<String>,
    total_keys_overflow: &mut bool,
    checks: &mut Vec<Check>,
) {
    if !path.is_file() {
        checks.push(Check::new(
            "config.credentials_file",
            CheckStatus::Fail,
            field,
            "Configured credential file is missing or not a regular file.",
            "Provide a readable 0600 regular file at the configured path.",
        ));
        return;
    }
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => {
            checks.push(Check::new(
                "config.credentials_file",
                CheckStatus::Fail,
                field,
                "Configured credential file metadata cannot be read.",
                "Ensure the configured credential file is accessible.",
            ));
            return;
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            checks.push(Check::new(
                "config.credentials_file",
                CheckStatus::Fail,
                field,
                "Configured credential file has insecure permissions (must not have group or other permissions).",
                "Set file permissions to 0600 or stricter (chmod 600).",
            ));
            return;
        }
    }
    if metadata.len() > crate::config::MAX_CREDENTIAL_FILE_BYTES {
        checks.push(Check::new(
            "config.credentials_file",
            CheckStatus::Fail,
            field,
            "Configured credential file exceeds maximum allowed size of 64 KiB.",
            "Reduce the credential file size to 64 KiB or less.",
        ));
        return;
    }
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => {
            checks.push(Check::new(
                "config.credentials_file",
                CheckStatus::Fail,
                field,
                "Configured credential file is not valid UTF-8.",
                "Ensure the credential file is encoded as UTF-8.",
            ));
            return;
        }
    };

    for (line_idx, line) in content.lines().enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            let _ = line_idx;
            checks.push(Check::new(
                "config.credentials_file",
                CheckStatus::Fail,
                field,
                "Configured credential file contains an invalid line missing '='.",
                "Format lines as KEY=value.",
            ));
            return;
        };
        if !crate::config::is_valid_env_key(key) {
            checks.push(Check::new(
                "config.credentials_file",
                CheckStatus::Fail,
                field,
                "Configured credential file contains an invalid variable name.",
                "Use valid variable names matching ^[A-Za-z_][A-Za-z0-9_]{0,63}$.",
            ));
            return;
        }
        if key.starts_with("SHIPBRAID_") {
            checks.push(Check::new(
                "config.credentials_file",
                CheckStatus::Fail,
                field,
                "Configured credential file contains a variable with reserved prefix 'SHIPBRAID_'.",
                "Credential variable names must not start with 'SHIPBRAID_'.",
            ));
            return;
        }
        if value.is_empty() {
            checks.push(Check::new(
                "config.credentials_file",
                CheckStatus::Fail,
                field,
                "Configured credential file contains an empty variable value.",
                "Credential values must be non-empty.",
            ));
            return;
        }
        if value.len() > crate::config::MAX_CREDENTIAL_VALUE_BYTES {
            checks.push(Check::new(
                "config.credentials_file",
                CheckStatus::Fail,
                field,
                "Configured credential file contains a value exceeding maximum allowed size of 8192 bytes.",
                "Reduce the credential value length to 8192 bytes or less.",
            ));
            return;
        }
        if value.contains('\0') {
            checks.push(Check::new(
                "config.credentials_file",
                CheckStatus::Fail,
                field,
                "Configured credential file contains a null byte in a value.",
                "Credential values must not contain null bytes.",
            ));
            return;
        }
        if allow_set.contains(key) {
            checks.push(Check::new(
                "config.credentials_file",
                CheckStatus::Fail,
                field,
                "Configured credential key conflicts with hook_environment.allow list.",
                "Credential keys must not intersect with the allowlist.",
            ));
            return;
        }
        if !all_keys.insert(key.to_string()) {
            checks.push(Check::new(
                "config.credentials_file",
                CheckStatus::Fail,
                field,
                "Configured credential file contains duplicate key across files.",
                "Ensure credential keys are unique across all configured environment files.",
            ));
            return;
        }
    }

    if all_keys.len() > crate::config::MAX_TOTAL_CREDENTIAL_KEYS {
        *total_keys_overflow = true;
    }

    checks.push(Check::new(
        "config.credentials_file",
        CheckStatus::Pass,
        field,
        "Configured credential file is present, has secure permissions, and is valid.",
        "No action is needed.",
    ));
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
            guide: None,
            credentials: None,
            hook_environment: None,
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

    #[test]
    fn guide_files_are_checked_for_presence_size_and_secrets_without_revealing_content() {
        let temp = tempfile::tempdir().unwrap();
        let good_file = temp.path().join("good.md");
        fs::write(&good_file, "# Benign documentation\nSecret canary: safe").unwrap();
        let missing_file = temp.path().join("missing.md");
        let large_file = temp.path().join("large.md");
        fs::write(
            &large_file,
            vec![b'x'; crate::config::MAX_GUIDE_FILE_BYTES + 10],
        )
        .unwrap();
        let secret_file = temp.path().join("secret.md");
        let secret_canary = "ghp_1234567890123456";
        fs::write(&secret_file, format!("Token: {secret_canary}")).unwrap();

        let mut lane = lane_with(LaneExecutorConfig::Scripted, None);
        lane.guide = Some(crate::config::GuideConfig {
            project: Some(good_file),
            environment: Some(missing_file),
            components: std::collections::BTreeMap::from([
                ("app".into(), large_file),
                ("secret".into(), secret_file),
            ]),
        });

        let config = Config {
            lanes: vec![lane],
            ..Config::default()
        };

        let checks = configuration_checks_with(&config, &|_| Err(VarError::NotPresent), None);

        assert_eq!(
            status_for(&checks, "lanes[0].guide.project"),
            CheckStatus::Pass
        );
        assert_eq!(
            status_for(&checks, "lanes[0].guide.environment"),
            CheckStatus::Fail
        );
        assert_eq!(
            status_for(&checks, "lanes[0].guide.components.app"),
            CheckStatus::Fail
        );
        assert_eq!(
            status_for(&checks, "lanes[0].guide.components.secret"),
            CheckStatus::Fail
        );

        let report = checks
            .iter()
            .map(|check| format!("{} {} {}", check.field, check.message, check.suggestion))
            .collect::<String>();
        assert!(
            !report.contains(secret_canary),
            "doctor probe must not leak secret content"
        );
        assert!(
            !report.contains("safe"),
            "doctor probe must not output benign file content"
        );
    }

    #[test]
    fn guide_doctor_checks_invalid_utf8_and_exact_size_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let exact_size_file = temp.path().join("exact.md");
        fs::write(
            &exact_size_file,
            vec![b'#'; crate::config::MAX_GUIDE_FILE_BYTES],
        )
        .unwrap();

        let oversized_file = temp.path().join("oversized.md");
        fs::write(
            &oversized_file,
            vec![b'#'; crate::config::MAX_GUIDE_FILE_BYTES + 1],
        )
        .unwrap();

        let non_utf8_file = temp.path().join("invalid_utf8.md");
        fs::write(&non_utf8_file, [0xFF, 0xFE, 0xFD, 0x80]).unwrap();

        let mut lane = lane_with(LaneExecutorConfig::Scripted, None);
        lane.guide = Some(crate::config::GuideConfig {
            project: Some(exact_size_file),
            environment: Some(oversized_file),
            components: std::collections::BTreeMap::from([("non_utf8".into(), non_utf8_file)]),
        });

        let config = Config {
            lanes: vec![lane],
            ..Config::default()
        };

        let checks = configuration_checks_with(&config, &|_| Err(VarError::NotPresent), None);

        assert_eq!(
            status_for(&checks, "lanes[0].guide.project"),
            CheckStatus::Pass
        );
        assert_eq!(
            status_for(&checks, "lanes[0].guide.environment"),
            CheckStatus::Fail
        );
        assert_eq!(
            status_for(&checks, "lanes[0].guide.components.non_utf8"),
            CheckStatus::Fail
        );

        let non_utf8_check = checks
            .iter()
            .find(|c| c.field == "lanes[0].guide.components.non_utf8")
            .unwrap();
        assert!(non_utf8_check.message.contains("not valid UTF-8"));
    }

    #[test]
    fn credential_files_and_hook_environments_diagnostics_checks() {
        let temp = tempfile::tempdir().unwrap();
        let good_cred = temp.path().join("good.env");
        fs::write(
            &good_cred,
            "API_KEY=secret_canary_value_1234\nDB_PASS=db_secret_5678\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&good_cred, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let insecure_cred = temp.path().join("insecure.env");
        fs::write(&insecure_cred, "FOO=bar\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&insecure_cred, fs::Permissions::from_mode(0o666)).unwrap();
        }

        let command_hook = HookConfig::Command {
            program: "/bin/sh".into(),
            args: vec![],
            cwd: None,
            timeout_ms: 1000,
        };

        // 1. Hooks lane with good cred and isolated hook environment
        let mut lane_isolated = lane_with(
            LaneExecutorConfig::Hooks {
                dispatch: command_hook.clone(),
                observe: command_hook.clone(),
                verify: command_hook.clone(),
            },
            None,
        );
        lane_isolated.credentials = Some(crate::config::CredentialsConfig {
            env_files: vec![good_cred.clone()],
        });
        lane_isolated.hook_environment = Some(crate::config::HookEnvironmentConfig {
            mode: crate::config::HookEnvironmentMode::Isolated,
            allow: vec!["PATH".into(), "HOME".into()],
        });

        // 2. Hooks lane with insecure cred and inherit mode
        let mut lane_inherit = lane_with(
            LaneExecutorConfig::Hooks {
                dispatch: command_hook.clone(),
                observe: command_hook.clone(),
                verify: command_hook,
            },
            None,
        );
        lane_inherit.credentials = Some(crate::config::CredentialsConfig {
            env_files: vec![insecure_cred.clone()],
        });

        // 3. Scripted lane with credentials -> fail
        let mut lane_scripted = lane_with(LaneExecutorConfig::Scripted, None);
        lane_scripted.credentials = Some(crate::config::CredentialsConfig {
            env_files: vec![good_cred.clone()],
        });

        let config = Config {
            lanes: vec![lane_isolated, lane_inherit, lane_scripted],
            ..Config::default()
        };

        let checks = configuration_checks_with(&config, &|_| Err(VarError::NotPresent), None);

        // Lane 0: good cred passes, isolated environment passes
        assert_eq!(
            status_for(&checks, "lanes[0].credentials.env_files[0]"),
            CheckStatus::Pass
        );
        assert_eq!(
            status_for(&checks, "lanes[0].hook_environment"),
            CheckStatus::Pass
        );

        // Lane 1: insecure cred fails (on unix), inherit environment warns
        #[cfg(unix)]
        assert_eq!(
            status_for(&checks, "lanes[1].credentials.env_files[0]"),
            CheckStatus::Fail
        );
        assert_eq!(
            status_for(&checks, "lanes[1].hook_environment"),
            CheckStatus::Warn
        );

        // Lane 2: scripted lane with credentials fails
        assert_eq!(
            status_for(&checks, "lanes[2].credentials"),
            CheckStatus::Fail
        );

        // Ensure no secret values leak into doctor check messages
        let report = checks
            .iter()
            .map(|check| format!("{} {} {}", check.field, check.message, check.suggestion))
            .collect::<String>();
        assert!(
            !report.contains("secret_canary_value_1234"),
            "doctor probe must not leak credential value"
        );
        assert!(
            !report.contains("db_secret_5678"),
            "doctor probe must not leak credential value"
        );
    }
}
