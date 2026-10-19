use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use crate::domain::{ParameterSpec, validate_parameter_specs};

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub mode: ConfigMode,
    pub database_path: PathBuf,
    pub listen: SocketAddr,
    pub tokens: Vec<TokenConfig>,
    pub lanes: Vec<LaneConfig>,
    pub max_active_runs: usize,
    pub tick_ms: u64,
    pub scripted_phase_ms: u64,
    pub cancel_timeout_seconds: u64,
    pub coalesce_ms: u64,
    pub dashboard_dir: Option<PathBuf>,
    pub artifacts_dir: Option<PathBuf>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            mode: ConfigMode::Server,
            database_path: "./data/shipbraid.db".into(),
            listen: "127.0.0.1:8787".parse().unwrap(),
            tokens: vec![],
            lanes: vec![],
            max_active_runs: 2,
            tick_ms: 250,
            scripted_phase_ms: 3000,
            cancel_timeout_seconds: 60,
            coalesce_ms: 2000,
            dashboard_dir: None,
            artifacts_dir: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfigMode {
    Local,
    #[default]
    Server,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenConfig {
    pub id: String,
    pub token_env: String,
    #[serde(default)]
    pub lanes: Vec<String>,
    #[serde(default)]
    pub projects: Vec<String>,
    #[serde(default)]
    pub urgent: bool,
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub executor: bool,
}
#[derive(Clone)]
pub struct ResolvedToken {
    pub id: String,
    pub token_hash: [u8; 32],
    pub lanes: Vec<String>,
    pub projects: Vec<String>,
    pub urgent: bool,
    pub admin: bool,
    pub executor: bool,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneConfig {
    pub id: String,
    pub project: String,
    pub environment: String,
    pub repository_id: String,
    pub repository_path: PathBuf,
    pub release_ref: String,
    pub profile_digest: String,
    pub components: Vec<String>,
    #[serde(default)]
    pub request_parameters: BTreeMap<String, ParameterSpec>,
    #[serde(default)]
    pub allow_descendant: bool,
    #[serde(default)]
    pub allow_preemption: bool,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub executor: LaneExecutorConfig,
    /// Read-only, failure-only reconciliation. Never dispatches or verifies a
    /// deployment, so it does not replace the frozen execution profile.
    #[serde(default)]
    pub failure_observe: Option<HookConfig>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(
    clippy::large_enum_variant,
    reason = "Administrator configuration is loaded once; keep named inline hook fields in its serde schema"
)]
pub enum LaneExecutorConfig {
    #[default]
    Scripted,
    Hooks {
        dispatch: HookConfig,
        observe: HookConfig,
        verify: HookConfig,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookConfig {
    Command {
        program: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        timeout_ms: u64,
    },
    Script {
        path: PathBuf,
        interpreter: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        timeout_ms: u64,
    },
    Webhook {
        url: String,
        token_env: String,
        timeout_ms: u64,
    },
}

impl LaneExecutorConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Scripted => "scripted",
            Self::Hooks { .. } => "hooks",
        }
    }

    pub fn hooks(&self) -> Option<(&HookConfig, &HookConfig, &HookConfig)> {
        match self {
            Self::Scripted => None,
            Self::Hooks {
                dispatch,
                observe,
                verify,
            } => Some((dispatch, observe, verify)),
        }
    }
}
fn default_enabled() -> bool {
    true
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let config = Self::load_unvalidated(path)?;
        config.validate()?;
        Ok(config)
    }

    /// Resolve configuration references for offline diagnostics. Runtime callers
    /// must continue using `load`, which also enforces semantic validation.
    pub(crate) fn load_unvalidated(path: &Path) -> Result<Self> {
        let path = path
            .canonicalize()
            .context("cannot resolve configuration path")?;
        let mut config: Self =
            toml::from_str(&std::fs::read_to_string(&path).context("cannot read configuration")?)
                .context("invalid TOML configuration")?;
        let base = path
            .parent()
            .context("configuration needs a parent directory")?;
        if config.database_path.is_relative() {
            config.database_path = base.join(&config.database_path);
        }
        if let Some(dashboard_dir) = &mut config.dashboard_dir
            && dashboard_dir.is_relative()
        {
            *dashboard_dir = base.join(&*dashboard_dir);
        }
        if let Some(artifacts_dir) = &mut config.artifacts_dir
            && artifacts_dir.is_relative()
        {
            *artifacts_dir = base.join(&*artifacts_dir);
        }
        for lane in &mut config.lanes {
            if lane.repository_path.is_relative() {
                lane.repository_path = base.join(&lane.repository_path);
            }
            if let LaneExecutorConfig::Hooks {
                dispatch,
                observe,
                verify,
            } = &mut lane.executor
            {
                for hook in [dispatch, observe, verify]
                    .into_iter()
                    .chain(lane.failure_observe.iter_mut())
                {
                    match hook {
                        HookConfig::Command { program, cwd, .. } => {
                            resolve_program(base, program);
                            if let Some(cwd) = cwd {
                                resolve_path(base, cwd);
                            }
                        }
                        HookConfig::Script {
                            path,
                            interpreter,
                            cwd,
                            ..
                        } => {
                            resolve_path(base, path);
                            resolve_program(base, interpreter);
                            if let Some(cwd) = cwd {
                                resolve_path(base, cwd);
                            }
                        }
                        HookConfig::Webhook { .. } => {}
                    }
                }
            }
        }
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        if self.max_active_runs == 0
            || self.tick_ms == 0
            || self.scripted_phase_ms == 0
            || self.cancel_timeout_seconds == 0
        {
            bail!("concurrency and timing settings must be positive");
        }
        match self.mode {
            ConfigMode::Local => {
                if !self.tokens.is_empty() {
                    bail!("local mode does not allow API tokens");
                }
                if self.dashboard_dir.is_some() {
                    bail!("local mode does not allow a dashboard directory");
                }
                if self.artifacts_dir.is_some() {
                    bail!("local mode does not allow an artifacts directory");
                }
                if self.lanes.is_empty() {
                    bail!("local mode requires at least one lane");
                }
                let project = &self.lanes[0].project;
                if self
                    .lanes
                    .iter()
                    .any(|lane| lane.project.as_str() != project)
                {
                    bail!("local mode lanes must belong to one project");
                }
            }
            ConfigMode::Server if self.tokens.is_empty() => {
                bail!("at least one authentication token must be configured");
            }
            ConfigMode::Server => {}
        }
        if self
            .dashboard_dir
            .as_ref()
            .is_some_and(|path| !path.is_dir())
        {
            bail!("configured dashboard directory does not exist");
        }
        if self
            .artifacts_dir
            .as_ref()
            .is_some_and(|path| path.exists() && !path.is_dir())
        {
            bail!("configured artifacts directory is not a directory");
        }
        let mut lane_ids = std::collections::HashSet::new();
        for lane in &self.lanes {
            if lane.id.is_empty()
                || lane.project.is_empty()
                || lane.environment.is_empty()
                || lane.repository_id.is_empty()
                || lane.profile_digest.is_empty()
                || lane.components.is_empty()
                || lane.release_ref.is_empty()
            {
                bail!(
                    "lane identity, repository, profile, release ref and components are required"
                );
            }
            if !lane_ids.insert(&lane.id) {
                bail!("duplicate lane id: {}", lane.id);
            }
            if !lane.repository_path.is_dir() {
                bail!(
                    "configured repository directory missing for lane {}",
                    lane.id
                );
            }
            validate_parameter_specs(&lane.request_parameters).map_err(|error| {
                anyhow::anyhow!("invalid request parameters for lane {}: {error}", lane.id)
            })?;
            if lane.executor.kind() == "hooks" && lane.allow_preemption {
                bail!("hook lane {} must disable preemption", lane.id);
            }
            if let Some(hook) = &lane.failure_observe {
                if lane.executor.kind() != "hooks" {
                    bail!("failure observer requires a hooks lane: {}", lane.id);
                }
                validate_hook(&lane.id, "failure_observe", hook)?;
            }
            if let Some((dispatch, observe, verify)) = lane.executor.hooks() {
                for (name, hook) in [
                    ("dispatch", dispatch),
                    ("observe", observe),
                    ("verify", verify),
                ] {
                    validate_hook(&lane.id, name, hook)?;
                }
            }
        }
        let mut token_ids = std::collections::HashSet::new();
        for token in &self.tokens {
            if token.id.is_empty() || token.token_env.is_empty() || !token_ids.insert(&token.id) {
                bail!("token ids and environment variable names must be nonempty and unique");
            }
            if token.lanes.iter().any(|id| !lane_ids.contains(id)) {
                bail!("token {} names an unknown lane", token.id);
            }
            if !token.admin && token.lanes.is_empty() && token.projects.is_empty() {
                bail!(
                    "non-administrator token {} needs a lane or project scope",
                    token.id
                );
            }
        }
        Ok(())
    }
    pub fn resolve_tokens(&self) -> Result<Vec<ResolvedToken>> {
        let mut seen = std::collections::HashSet::new();
        self.tokens
            .iter()
            .map(|token| {
                let secret = std::env::var(&token.token_env).with_context(|| {
                    format!("missing token environment variable {}", token.token_env)
                })?;
                if secret.len() < 32 {
                    bail!("token {} needs at least 32 bytes", token.id);
                }
                let hash: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
                if !seen.insert(hash) {
                    bail!("tokens must have distinct credential values");
                }
                Ok(ResolvedToken {
                    id: token.id.clone(),
                    token_hash: hash,
                    lanes: token.lanes.clone(),
                    projects: token.projects.clone(),
                    urgent: token.urgent,
                    admin: token.admin,
                    executor: token.executor,
                })
            })
            .collect()
    }

    pub fn effective_profile_digest(&self, lane: &LaneConfig) -> Result<String> {
        let executor_profile = match &lane.executor {
            LaneExecutorConfig::Scripted => None,
            LaneExecutorConfig::Hooks { .. } => Some(
                serde_json::to_vec(&lane.executor)
                    .context("serialize configured executor profile")?,
            ),
        };
        if executor_profile.is_none() && lane.request_parameters.is_empty() {
            return Ok(lane.profile_digest.clone());
        }
        let mut digest = Sha256::new();
        digest.update(lane.profile_digest.as_bytes());
        if let Some(executor_profile) = executor_profile {
            digest.update([0]);
            digest.update(executor_profile);
        }
        if !lane.request_parameters.is_empty() {
            let schema = serde_json::to_vec(&lane.request_parameters)
                .context("serialize configured request parameter schema")?;
            digest.update([0]);
            digest.update(schema);
        }
        Ok(hex::encode(digest.finalize()))
    }
}

fn resolve_path(base: &Path, path: &mut PathBuf) {
    if path.is_relative() {
        *path = base.join(&*path);
    }
}

fn resolve_program(base: &Path, path: &mut PathBuf) {
    if path.is_relative() && path.components().count() > 1 {
        *path = base.join(&*path);
    }
}

fn validate_hook(lane: &str, operation: &str, hook: &HookConfig) -> Result<()> {
    let timeout_ms = match hook {
        HookConfig::Command {
            program,
            args,
            cwd,
            timeout_ms,
        } => {
            if program.as_os_str().is_empty()
                || args.len() > 64
                || cwd.as_ref().is_some_and(|path| !path.is_dir())
            {
                bail!("invalid {operation} command hook for lane {lane}");
            }
            validate_hook_args(lane, operation, args)?;
            *timeout_ms
        }
        HookConfig::Script {
            path,
            interpreter,
            args,
            cwd,
            timeout_ms,
        } => {
            if !path.is_file()
                || interpreter.as_os_str().is_empty()
                || args.len() > 64
                || cwd.as_ref().is_some_and(|path| !path.is_dir())
            {
                bail!("invalid {operation} script hook for lane {lane}");
            }
            validate_hook_args(lane, operation, args)?;
            *timeout_ms
        }
        HookConfig::Webhook {
            url,
            token_env,
            timeout_ms,
        } => {
            let parsed = reqwest::Url::parse(url)
                .with_context(|| format!("invalid {operation} webhook URL for lane {lane}"))?;
            let host = parsed.host_str().unwrap_or_default();
            if !matches!(parsed.scheme(), "http" | "https")
                || host.is_empty()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.fragment().is_some()
                || token_env.trim().is_empty()
                || (parsed.scheme() == "http" && !is_loopback_host(host))
            {
                bail!("invalid {operation} webhook configuration for lane {lane}");
            }
            *timeout_ms
        }
    };
    if !(100..=300_000).contains(&timeout_ms) {
        bail!("{operation} hook timeout for lane {lane} must be between 100 and 300000 ms");
    }
    Ok(())
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn validate_hook_args(lane: &str, operation: &str, args: &[String]) -> Result<()> {
    if args
        .iter()
        .any(|arg| arg.len() > 4096 || arg.contains('\0'))
    {
        bail!("{operation} hook arguments for lane {lane} exceed configured limits");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_lane(id: &str, project: &str, repository_path: &Path) -> LaneConfig {
        LaneConfig {
            id: id.into(),
            project: project.into(),
            environment: "test".into(),
            repository_id: format!("{project}-repository"),
            repository_path: repository_path.to_path_buf(),
            release_ref: "refs/heads/main".into(),
            profile_digest: "profile".into(),
            components: vec!["app".into()],
            request_parameters: BTreeMap::new(),
            allow_descendant: false,
            allow_preemption: false,
            enabled: true,
            executor: LaneExecutorConfig::Scripted,
            failure_observe: None,
        }
    }

    #[test]
    fn omitted_mode_preserves_server_and_local_mode_is_single_project_without_api_surface() {
        let legacy: Config = toml::from_str("").unwrap();
        assert_eq!(legacy.mode, ConfigMode::Server);
        assert!(legacy.validate().is_err(), "server still requires a token");

        let temp = tempfile::tempdir().unwrap();
        let base = Config {
            mode: ConfigMode::Local,
            lanes: vec![local_lane("dev", "project", temp.path())],
            ..Config::default()
        };
        assert!(base.validate().is_ok(), "local mode needs no API token");

        let mut multiple_environments = base.clone();
        let mut second = local_lane("prod", "project", temp.path());
        second.environment = "prod".into();
        multiple_environments.lanes.push(second);
        assert!(multiple_environments.validate().is_ok());

        let mut cross_project = base.clone();
        cross_project
            .lanes
            .push(local_lane("other", "other-project", temp.path()));
        assert!(cross_project.validate().is_err());

        let mut with_token = base.clone();
        with_token.tokens.push(TokenConfig {
            id: "operator".into(),
            token_env: "TOKEN".into(),
            lanes: vec!["dev".into()],
            projects: vec![],
            urgent: false,
            admin: false,
            executor: false,
        });
        assert!(with_token.validate().is_err());

        let mut with_dashboard = base.clone();
        with_dashboard.dashboard_dir = Some(temp.path().to_path_buf());
        assert!(with_dashboard.validate().is_err());

        let mut with_artifacts = base;
        with_artifacts.artifacts_dir = Some(temp.path().to_path_buf());
        assert!(with_artifacts.validate().is_err());
    }

    #[test]
    fn rejects_zero_capacity_and_missing_auth() {
        assert!(Config::default().validate().is_err());
        let config = Config {
            max_active_runs: 0,
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }
    #[test]
    fn missing_lane_enabled_defaults_on() {
        let input = "id='test'\nproject='p'\nenvironment='test'\nrepository_id='r'\nrepository_path='.'\nrelease_ref='HEAD'\nprofile_digest='one'\ncomponents=['app']";
        let lane: LaneConfig = toml::from_str(input).unwrap();
        assert!(lane.enabled);
        assert!(!lane.allow_descendant);
        assert!(!lane.allow_preemption);
        assert!(lane.request_parameters.is_empty());
        assert_eq!(lane.executor.kind(), "scripted");
    }

    #[test]
    fn request_parameter_schema_defaults_and_config_validation_are_strict() {
        let temp = tempfile::tempdir().unwrap();
        let lane: LaneConfig = toml::from_str(&format!(
            "id='test'\nproject='p'\nenvironment='test'\nrepository_id='r'\nrepository_path='{}'\nrelease_ref='HEAD'\nprofile_digest='base'\ncomponents=['app']\n[request_parameters.database_backup]\nallowed_values=['include','skip']",
            temp.path().display()
        ))
        .unwrap();
        let spec = &lane.request_parameters["database_backup"];
        assert!(spec.required, "request parameters are required by default");
        assert_eq!(spec.allowed_values, ["include", "skip"]);

        let mut invalid_lane = lane.clone();
        invalid_lane
            .request_parameters
            .get_mut("database_backup")
            .unwrap()
            .allowed_values = (0..33).map(|index| format!("choice-{index}")).collect();
        let mut config = Config {
            lanes: vec![invalid_lane],
            tokens: vec![TokenConfig {
                id: "operator".into(),
                token_env: "OPERATOR_TOKEN".into(),
                lanes: vec!["test".into()],
                projects: vec![],
                urgent: false,
                admin: false,
                executor: false,
            }],
            ..Config::default()
        };
        assert!(config.validate().is_err(), "schema enum size is bounded");

        config.lanes[0]
            .request_parameters
            .get_mut("database_backup")
            .unwrap()
            .allowed_values = vec!["include".into(), "skip".into()];
        assert!(config.validate().is_ok());
    }

    #[test]
    fn parameter_schema_changes_scripted_profile_but_empty_schema_preserves_legacy_digest() {
        let mut lane: LaneConfig = toml::from_str(
            "id='test'\nproject='p'\nenvironment='dev'\nrepository_id='r'\nrepository_path='.'\nrelease_ref='refs/heads/main'\nprofile_digest='base'\ncomponents=['app']",
        )
        .unwrap();
        let config = Config::default();
        assert_eq!(config.effective_profile_digest(&lane).unwrap(), "base");
        lane.request_parameters.insert(
            "database_backup".into(),
            ParameterSpec {
                required: true,
                allowed_values: vec!["include".into(), "skip".into()],
            },
        );
        let first = config.effective_profile_digest(&lane).unwrap();
        assert_ne!(first, "base");
        lane.request_parameters
            .get_mut("database_backup")
            .unwrap()
            .allowed_values = vec!["include".into()];
        assert_ne!(first, config.effective_profile_digest(&lane).unwrap());
    }

    #[test]
    fn webhook_urls_do_not_send_bearers_to_plaintext_remote_hosts() {
        let timeout = 1_000;
        assert!(
            validate_hook(
                "dev",
                "dispatch",
                &HookConfig::Webhook {
                    url: "http://127.0.0.1:9900/hook".into(),
                    token_env: "HOOK_TOKEN".into(),
                    timeout_ms: timeout,
                }
            )
            .is_ok()
        );
        assert!(
            validate_hook(
                "dev",
                "dispatch",
                &HookConfig::Webhook {
                    url: "http://example.com/hook".into(),
                    token_env: "HOOK_TOKEN".into(),
                    timeout_ms: timeout,
                }
            )
            .is_err()
        );
        assert!(
            validate_hook(
                "dev",
                "dispatch",
                &HookConfig::Webhook {
                    url: "https://example.com/hook".into(),
                    token_env: "HOOK_TOKEN".into(),
                    timeout_ms: timeout,
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn hook_profile_digest_changes_when_admin_transport_changes() {
        let temp = tempfile::tempdir().unwrap();
        let mut lane: LaneConfig = toml::from_str(&format!(
            "id='test'\nproject='p'\nenvironment='dev'\nrepository_id='r'\nrepository_path='{}'\nrelease_ref='refs/heads/main'\nprofile_digest='base'\ncomponents=['app']\nallow_preemption=false",
            temp.path().display()
        ))
        .unwrap();
        lane.executor = LaneExecutorConfig::Hooks {
            dispatch: HookConfig::Command {
                program: "/bin/true".into(),
                args: vec!["one".into()],
                cwd: None,
                timeout_ms: 1_000,
            },
            observe: HookConfig::Command {
                program: "/bin/true".into(),
                args: vec![],
                cwd: None,
                timeout_ms: 1_000,
            },
            verify: HookConfig::Command {
                program: "/bin/true".into(),
                args: vec![],
                cwd: None,
                timeout_ms: 1_000,
            },
        };
        let config = Config::default();
        let first = config.effective_profile_digest(&lane).unwrap();
        let serialized = serde_json::to_vec(&lane.executor).unwrap();
        let mut legacy_digest = Sha256::new();
        legacy_digest.update(lane.profile_digest.as_bytes());
        legacy_digest.update([0]);
        legacy_digest.update(serialized);
        assert_eq!(first, hex::encode(legacy_digest.finalize()));
        lane.failure_observe = Some(HookConfig::Command {
            program: "/bin/true".into(),
            args: vec![],
            cwd: None,
            timeout_ms: 1_000,
        });
        assert_eq!(
            first,
            config.effective_profile_digest(&lane).unwrap(),
            "failure-only observation cannot change the frozen dispatch profile"
        );

        lane.request_parameters.insert(
            "database_backup".into(),
            ParameterSpec {
                required: true,
                allowed_values: vec!["include".into(), "skip".into()],
            },
        );
        let schema_bound = config.effective_profile_digest(&lane).unwrap();
        assert_ne!(
            first, schema_bound,
            "request schemas are part of the lane profile"
        );
        lane.request_parameters.clear();
        assert_eq!(first, config.effective_profile_digest(&lane).unwrap());

        let LaneExecutorConfig::Hooks { dispatch, .. } = &mut lane.executor else {
            unreachable!()
        };
        *dispatch = HookConfig::Command {
            program: "/bin/true".into(),
            args: vec!["two".into()],
            cwd: None,
            timeout_ms: 1_000,
        };
        assert_ne!(first, config.effective_profile_digest(&lane).unwrap());
    }
}
