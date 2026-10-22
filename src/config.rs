use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use crate::domain::{ParameterSpec, validate_parameter_specs};
use crate::resources::{ResourceAdmissionConfig, ResourceRootConfig};

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
    pub resource_roots: Vec<ResourceRootConfig>,
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
            resource_roots: vec![],
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
    /// Optional, administrator-defined filesystem admission for this lane.
    #[serde(default)]
    pub resource_admission: Option<ResourceAdmissionConfig>,
    /// Optional natural-language release guide for this lane.
    #[serde(default)]
    pub guide: Option<GuideConfig>,
}

pub const MAX_GUIDE_FILE_BYTES: usize = 32768;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GuideConfig {
    #[serde(default)]
    pub project: Option<PathBuf>,
    #[serde(default)]
    pub environment: Option<PathBuf>,
    #[serde(default)]
    pub components: BTreeMap<String, PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideDocument {
    pub path: PathBuf,
    pub content_sha256: String,
    pub content: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaneGuideSnapshot {
    pub project: Option<GuideDocument>,
    pub environment: Option<GuideDocument>,
    pub components: BTreeMap<String, GuideDocument>,
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
            if let Some(guide) = &mut lane.guide {
                if let Some(project) = &mut guide.project {
                    resolve_path(base, project);
                }
                if let Some(environment) = &mut guide.environment {
                    resolve_path(base, environment);
                }
                for path in guide.components.values_mut() {
                    resolve_path(base, path);
                }
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
        for root in &mut config.resource_roots {
            if root.path.is_relative() {
                root.path = base.join(&root.path);
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
        let mut resource_root_ids = std::collections::HashSet::new();
        for root in &self.resource_roots {
            if crate::resources::validate_resource_id(&root.id).is_err()
                || root.managed_budget_bytes == 0
                || !root.path.is_absolute()
                || !root.path.is_dir()
            {
                bail!(
                    "resource roots need a safe unique id, positive managed budget, and existing absolute directory"
                );
            }
            if !resource_root_ids.insert(&root.id) {
                bail!("duplicate resource root id: {}", root.id);
            }
        }
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
            if let Some(admission) = &lane.resource_admission {
                if admission.reservation_bytes == 0 {
                    bail!(
                        "resource admission reservation for lane {} must be positive",
                        lane.id
                    );
                }
                if !resource_root_ids.contains(&admission.root_id) {
                    bail!(
                        "lane {} names an unknown resource root: {}",
                        lane.id,
                        admission.root_id
                    );
                }
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
            if let Some(guide) = &lane.guide {
                guide.load_snapshot(&lane.id, &lane.components)?;
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

    pub fn load_guides(&self) -> Result<BTreeMap<String, LaneGuideSnapshot>> {
        let mut snapshots = BTreeMap::new();
        for lane in &self.lanes {
            if let Some(guide) = &lane.guide {
                snapshots.insert(
                    lane.id.clone(),
                    guide.load_snapshot(&lane.id, &lane.components)?,
                );
            }
        }
        Ok(snapshots)
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

impl GuideConfig {
    pub fn load_snapshot(
        &self,
        lane_id: &str,
        lane_components: &[String],
    ) -> Result<LaneGuideSnapshot> {
        for component in self.components.keys() {
            if !lane_components.contains(component) {
                bail!("guide component '{component}' is not declared in lane '{lane_id}'");
            }
        }
        let project = self
            .project
            .as_deref()
            .map(|path| load_guide_document(lane_id, "project", path))
            .transpose()?;
        let environment = self
            .environment
            .as_deref()
            .map(|path| load_guide_document(lane_id, "environment", path))
            .transpose()?;
        let mut components = BTreeMap::new();
        for (name, path) in &self.components {
            components.insert(name.clone(), load_guide_document(lane_id, name, path)?);
        }
        Ok(LaneGuideSnapshot {
            project,
            environment,
            components,
        })
    }
}

fn load_guide_document(lane_id: &str, label: &str, path: &Path) -> Result<GuideDocument> {
    if !path.is_file() {
        bail!(
            "lane '{lane_id}' {label} guide file '{}' does not exist or is not a regular file",
            path.display()
        );
    }
    let metadata = path
        .metadata()
        .with_context(|| format!("cannot read metadata of guide file '{}'", path.display()))?;
    if metadata.len() > MAX_GUIDE_FILE_BYTES as u64 {
        bail!(
            "lane '{lane_id}' {label} guide file '{}' exceeds maximum allowed size of {} bytes",
            path.display(),
            MAX_GUIDE_FILE_BYTES
        );
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read guide file '{}'", path.display()))?;
    if let Some(pattern) = scan_secret(&content) {
        bail!(
            "lane '{lane_id}' {label} guide file '{}' contains prohibited secret pattern ({pattern})",
            path.display()
        );
    }
    let content_sha256 = hex::encode(Sha256::digest(content.as_bytes()));
    Ok(GuideDocument {
        path: path.to_path_buf(),
        content_sha256,
        content,
    })
}

const TOKEN_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxr-",
    "xoxs-",
    "sk-ant-",
];

pub fn scan_secret(content: &str) -> Option<&'static str> {
    if has_pem_private_key(content) {
        return Some("PEM private key");
    }
    if has_aws_access_key(content) {
        return Some("AWS access key ID");
    }
    if has_google_api_key(content) {
        return Some("Google API key");
    }
    for prefix in TOKEN_PREFIXES {
        if has_token_prefix(content, prefix) {
            return Some("prohibited token credential");
        }
    }
    None
}

fn has_pem_private_key(text: &str) -> bool {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(idx) = trimmed.find("-----BEGIN ") {
            let rest = &trimmed[idx + "-----BEGIN ".len()..];
            if rest.contains("PRIVATE KEY-----") {
                return true;
            }
        }
    }
    false
}

fn has_aws_access_key(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() < 20 {
        return false;
    }
    for i in 0..=bytes.len() - 20 {
        if &bytes[i..i + 4] == b"AKIA" {
            let tail = &bytes[i + 4..i + 20];
            if tail
                .iter()
                .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
            {
                return true;
            }
        }
    }
    false
}

fn has_google_api_key(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() < 39 {
        return false;
    }
    for i in 0..=bytes.len() - 39 {
        if &bytes[i..i + 4] == b"AIza" {
            let tail = &bytes[i + 4..i + 39];
            if tail
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
            {
                return true;
            }
        }
    }
    false
}

fn has_token_prefix(text: &str, prefix: &str) -> bool {
    let mut offset = 0;
    while let Some(pos) = text[offset..].find(prefix) {
        let abs_pos = offset + pos;
        let suffix = &text[abs_pos + prefix.len()..];
        let non_ws_count = suffix.chars().take_while(|c| !c.is_whitespace()).count();
        if non_ws_count >= 16 {
            return true;
        }
        offset = abs_pos + prefix.len();
    }
    false
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
            resource_admission: None,
            guide: None,
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

    #[test]
    fn secret_scanner_detects_prohibited_patterns_and_ignores_benign_content() {
        // Benign content
        assert_eq!(
            scan_secret("# Deployment Guide\nDeploy with `cargo run`.\n"),
            None
        );
        assert_eq!(scan_secret("-----BEGIN CERTIFICATE-----\nMIIB..."), None);
        assert_eq!(scan_secret("-----BEGIN PUBLIC KEY-----\nMIIB..."), None);
        assert_eq!(scan_secret("AKIA12345"), None); // Too short
        assert_eq!(scan_secret("AKIA1234567890abcdef"), None); // Lowercase
        assert_eq!(scan_secret("AIzaSyD-1234"), None); // Too short
        assert_eq!(scan_secret("ghp_short"), None); // < 16 non-whitespace

        // PEM private keys
        assert_eq!(
            scan_secret("-----BEGIN RSA PRIVATE KEY-----\nMIIE..."),
            Some("PEM private key")
        );
        assert_eq!(
            scan_secret("-----BEGIN EC PRIVATE KEY-----\nMHQC..."),
            Some("PEM private key")
        );
        assert_eq!(
            scan_secret("-----BEGIN OPENSSH PRIVATE KEY-----\nb3Blb..."),
            Some("PEM private key")
        );
        assert_eq!(
            scan_secret("-----BEGIN PRIVATE KEY-----\nMIIE..."),
            Some("PEM private key")
        );

        // AWS key
        assert_eq!(
            scan_secret("Set AWS_KEY=AKIAIOSFODNN7EXAMPLE in your env"),
            Some("AWS access key ID")
        );

        // Google API key
        assert_eq!(
            scan_secret("key: AIzaSyD-1234567890abcdef_ABCDEF12345678"),
            Some("Google API key")
        );

        // Token prefixes with >= 16 non-whitespace
        assert_eq!(
            scan_secret("token: ghp_1234567890123456"),
            Some("prohibited token credential")
        );
        assert_eq!(
            scan_secret("token: gho_1234567890123456"),
            Some("prohibited token credential")
        );
        assert_eq!(
            scan_secret("token: ghu_1234567890123456"),
            Some("prohibited token credential")
        );
        assert_eq!(
            scan_secret("token: ghs_1234567890123456"),
            Some("prohibited token credential")
        );
        assert_eq!(
            scan_secret("token: github_pat_11AAAAAAA0123456789012"),
            Some("prohibited token credential")
        );
        assert_eq!(
            scan_secret("token: glpat-1234567890abcdef"),
            Some("prohibited token credential")
        );
        assert_eq!(
            scan_secret("token: xoxb-123456789012-123456789012"),
            Some("prohibited token credential")
        );
        assert_eq!(
            scan_secret("token: xoxp-123456789012-123456789012"),
            Some("prohibited token credential")
        );
        assert_eq!(
            scan_secret("token: xoxa-123456789012-123456789012"),
            Some("prohibited token credential")
        );
        assert_eq!(
            scan_secret("token: xoxr-123456789012-123456789012"),
            Some("prohibited token credential")
        );
        assert_eq!(
            scan_secret("token: xoxs-123456789012-123456789012"),
            Some("prohibited token credential")
        );
        assert_eq!(
            scan_secret("token: sk-ant-api03-abcdefghijklmnop"),
            Some("prohibited token credential")
        );
    }

    #[test]
    fn guide_validation_and_loading_handles_sizes_secrets_and_subsets() {
        let temp = tempfile::tempdir().unwrap();
        let overview = temp.path().join("overview.md");
        let dev = temp.path().join("dev.md");
        let app = temp.path().join("app.md");
        std::fs::write(&overview, "# Overview").unwrap();
        std::fs::write(&dev, "# Dev Environment").unwrap();
        std::fs::write(&app, "# App Component").unwrap();

        let mut guide = GuideConfig {
            project: Some(overview.clone()),
            environment: Some(dev.clone()),
            components: BTreeMap::from([("app".into(), app.clone())]),
        };

        // Success case
        let snapshot = guide.load_snapshot("lane-1", &["app".into()]).unwrap();
        assert!(snapshot.project.is_some());
        assert_eq!(snapshot.project.unwrap().content, "# Overview");
        assert!(snapshot.environment.is_some());
        assert_eq!(snapshot.environment.unwrap().content, "# Dev Environment");
        assert_eq!(snapshot.components["app"].content, "# App Component");

        // Component not in lane.components
        assert!(guide.load_snapshot("lane-1", &["worker".into()]).is_err());

        // File does not exist
        let missing = temp.path().join("missing.md");
        guide.project = Some(missing);
        assert!(guide.load_snapshot("lane-1", &["app".into()]).is_err());
        guide.project = Some(overview.clone());

        // Oversized file (> 32768 bytes)
        let large = temp.path().join("large.md");
        std::fs::write(&large, vec![b'a'; MAX_GUIDE_FILE_BYTES + 1]).unwrap();
        guide.project = Some(large);
        assert!(guide.load_snapshot("lane-1", &["app".into()]).is_err());
        guide.project = Some(overview.clone());

        // Boundary size (exactly 32768 bytes)
        let boundary = temp.path().join("boundary.md");
        std::fs::write(&boundary, vec![b'b'; MAX_GUIDE_FILE_BYTES]).unwrap();
        guide.project = Some(boundary);
        assert!(guide.load_snapshot("lane-1", &["app".into()]).is_ok());
        guide.project = Some(overview.clone());

        // Secret scan hit rejects loading
        let secret_file = temp.path().join("secret.md");
        std::fs::write(&secret_file, "key: AKIAIOSFODNN7EXAMPLE").unwrap();
        guide.project = Some(secret_file);
        assert!(guide.load_snapshot("lane-1", &["app".into()]).is_err());
    }

    #[test]
    fn effective_profile_digest_is_invariant_under_guide_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let overview = temp.path().join("overview.md");
        std::fs::write(&overview, "# Overview").unwrap();

        let lane_without_guide = LaneConfig {
            id: "test".into(),
            project: "p".into(),
            environment: "dev".into(),
            repository_id: "r".into(),
            repository_path: temp.path().to_path_buf(),
            release_ref: "refs/heads/main".into(),
            profile_digest: "base".into(),
            components: vec!["app".into()],
            request_parameters: BTreeMap::new(),
            allow_descendant: false,
            allow_preemption: false,
            enabled: true,
            executor: LaneExecutorConfig::Scripted,
            failure_observe: None,
            resource_admission: None,
            guide: None,
        };

        let mut lane_with_guide = lane_without_guide.clone();
        lane_with_guide.guide = Some(GuideConfig {
            project: Some(overview),
            environment: None,
            components: BTreeMap::new(),
        });

        let config = Config::default();
        let digest_without = config
            .effective_profile_digest(&lane_without_guide)
            .unwrap();
        let digest_with = config.effective_profile_digest(&lane_with_guide).unwrap();
        assert_eq!(
            digest_without, digest_with,
            "effective_profile_digest must not change when guide is configured"
        );
    }

    #[test]
    fn secret_scanner_fixed_patterns_and_negative_cases() {
        // PEM variants
        assert_eq!(
            scan_secret("-----BEGIN RSA PRIVATE KEY-----\nMIIE..."),
            Some("PEM private key")
        );
        assert_eq!(
            scan_secret("-----BEGIN EC PRIVATE KEY-----\nMHQC..."),
            Some("PEM private key")
        );
        assert_eq!(
            scan_secret("-----BEGIN OPENSSH PRIVATE KEY-----\nb3Blb..."),
            Some("PEM private key")
        );
        assert_eq!(
            scan_secret("-----BEGIN DSA PRIVATE KEY-----\nMIIB..."),
            Some("PEM private key")
        );
        assert_eq!(
            scan_secret("-----BEGIN ENCRYPTED PRIVATE KEY-----\nMIIB..."),
            Some("PEM private key")
        );
        assert_eq!(
            scan_secret("-----BEGIN PRIVATE KEY-----\nMIIE..."),
            Some("PEM private key")
        );
        assert_eq!(
            scan_secret("-----BEGIN /* comment */ PRIVATE KEY-----"),
            Some("PEM private key")
        );

        // Negative PEM cases
        assert_eq!(
            scan_secret("-----BEGIN CERTIFICATE REQUEST-----\nMIIB..."),
            None
        );
        assert_eq!(scan_secret("-----BEGIN PUBLIC KEY-----\nMIIB..."), None);
        assert_eq!(scan_secret("-----BEGIN CERTIFICATE-----\nMIIB..."), None);

        // AWS Key positive and negatives
        assert_eq!(
            scan_secret("AKIAIOSFODNN7EXAMPLE"),
            Some("AWS access key ID")
        );
        assert_eq!(
            scan_secret("export AWS_ACCESS_KEY_ID=AKIA1234567890ABCDEF"),
            Some("AWS access key ID")
        );
        assert_eq!(scan_secret("AKIA1234567890ABCDE"), None); // 15 chars (short)
        assert_eq!(scan_secret("AKIA1234567890abcdef"), None); // Lowercase
        assert_eq!(scan_secret("AKIA123456789012345-"), None); // Hyphen

        // Google API Key positive and negatives
        assert_eq!(
            scan_secret("AIzaSyD-1234567890abcdef_ABCDEF12345678"),
            Some("Google API key")
        );
        assert_eq!(
            scan_secret("AIza1234567890123456789012345678901234"),
            None // 34 chars (short)
        );
        assert_eq!(
            scan_secret("AIza1234567890123456789012345678901234!"),
            None // Invalid symbol
        );

        // Token prefix exact boundaries (16 chars hit, 15 chars pass)
        let prefixes = [
            "ghp_",
            "gho_",
            "ghu_",
            "ghs_",
            "github_pat_",
            "glpat-",
            "xoxb-",
            "xoxp-",
            "xoxa-",
            "xoxr-",
            "xoxs-",
            "sk-ant-",
        ];
        for prefix in prefixes {
            let hit_str = format!("{prefix}0123456789abcdef"); // 16 chars
            assert_eq!(
                scan_secret(&hit_str),
                Some("prohibited token credential"),
                "expected prefix '{prefix}' with 16 chars to hit"
            );

            let pass_str = format!("{prefix}0123456789abcde"); // 15 chars
            assert_eq!(
                scan_secret(&pass_str),
                None,
                "expected prefix '{prefix}' with 15 chars not to hit"
            );
        }

        // Benign markdown constructs
        assert_eq!(
            scan_secret("Latest commit: 7b06621d0ab234567890abcdef1234567890abcd by test"),
            None
        );
        assert_eq!(
            scan_secret("[PR link](https://github.com/aak1247/shipbraid/pull/1234567890123456)"),
            None
        );
        assert_eq!(
            scan_secret("Refer to the ghp_ documentation for personal access tokens."),
            None
        );
        assert_eq!(
            scan_secret("| Component | Hash |\n| app | 550e8400-e29b-41d4-a716-446655440000 |\n"),
            None
        );
    }

    #[test]
    fn guide_size_boundary_32768_bytes_pass_and_32769_bytes_reject() {
        let temp = tempfile::tempdir().unwrap();
        let mirror = temp.path().join("repo.git");
        std::fs::create_dir_all(&mirror).unwrap();

        let pass_file = temp.path().join("pass_32768.md");
        std::fs::write(&pass_file, vec![b'a'; MAX_GUIDE_FILE_BYTES]).unwrap();

        let reject_file = temp.path().join("reject_32769.md");
        std::fs::write(&reject_file, vec![b'a'; MAX_GUIDE_FILE_BYTES + 1]).unwrap();

        // 1. Project level boundary
        let guide_pass_proj = GuideConfig {
            project: Some(pass_file.clone()),
            environment: None,
            components: BTreeMap::new(),
        };
        assert!(guide_pass_proj.load_snapshot("lane", &[]).is_ok());

        let guide_reject_proj = GuideConfig {
            project: Some(reject_file.clone()),
            environment: None,
            components: BTreeMap::new(),
        };
        let err = guide_reject_proj
            .load_snapshot("lane", &[])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("exceeds maximum allowed size of 32768 bytes"),
            "actual: {err}"
        );

        // 2. Environment level boundary
        let guide_pass_env = GuideConfig {
            project: None,
            environment: Some(pass_file.clone()),
            components: BTreeMap::new(),
        };
        assert!(guide_pass_env.load_snapshot("lane", &[]).is_ok());

        let guide_reject_env = GuideConfig {
            project: None,
            environment: Some(reject_file.clone()),
            components: BTreeMap::new(),
        };
        assert!(guide_reject_env.load_snapshot("lane", &[]).is_err());

        // 3. Component level boundary
        let guide_pass_comp = GuideConfig {
            project: None,
            environment: None,
            components: BTreeMap::from([("app".into(), pass_file.clone())]),
        };
        assert!(
            guide_pass_comp
                .load_snapshot("lane", &["app".into()])
                .is_ok()
        );

        let guide_reject_comp = GuideConfig {
            project: None,
            environment: None,
            components: BTreeMap::from([("app".into(), reject_file.clone())]),
        };
        assert!(
            guide_reject_comp
                .load_snapshot("lane", &["app".into()])
                .is_err()
        );

        // 4. Config::validate integration with boundary
        let mut lane = local_lane("lane", "project", &mirror);
        lane.guide = Some(guide_pass_proj);
        let mut config = Config {
            mode: ConfigMode::Local,
            lanes: vec![lane.clone()],
            ..Config::default()
        };
        assert!(config.validate().is_ok());

        lane.guide = Some(guide_reject_proj);
        config.lanes = vec![lane];
        assert!(config.validate().is_err());
    }

    #[test]
    fn guide_components_not_subset_of_lane_components_fails_validation() {
        let temp = tempfile::tempdir().unwrap();
        let mirror = temp.path().join("repo.git");
        std::fs::create_dir_all(&mirror).unwrap();

        let guide_file = temp.path().join("guide.md");
        std::fs::write(&guide_file, "# Guide").unwrap();

        let guide = GuideConfig {
            project: None,
            environment: None,
            components: BTreeMap::from([
                ("service".into(), guide_file.clone()),
                ("database".into(), guide_file.clone()),
            ]),
        };

        // Lane declares only service and worker; database is extraneous
        let err = guide
            .load_snapshot("test-lane", &["service".into(), "worker".into()])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("guide component 'database' is not declared in lane 'test-lane'"),
            "actual: {err}"
        );

        // Lane declares empty components; service is extraneous
        let empty_guide = GuideConfig {
            project: None,
            environment: None,
            components: BTreeMap::from([("service".into(), guide_file)]),
        };
        let err2 = empty_guide
            .load_snapshot("test-lane-empty", &[])
            .unwrap_err()
            .to_string();
        assert!(
            err2.contains("guide component 'service' is not declared in lane 'test-lane-empty'"),
            "actual: {err2}"
        );

        // Config::validate integration
        let mut lane = local_lane("test-lane", "project", &mirror);
        lane.components = vec!["service".into(), "worker".into()];
        lane.guide = Some(guide);
        let config = Config {
            mode: ConfigMode::Local,
            lanes: vec![lane],
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn effective_profile_digest_invariant_across_guide_mutations() {
        let temp = tempfile::tempdir().unwrap();
        let overview = temp.path().join("overview.md");
        std::fs::write(&overview, "# Initial Overview Content").unwrap();
        let dev = temp.path().join("dev.md");
        std::fs::write(&dev, "# Initial Dev Content").unwrap();
        let app = temp.path().join("app.md");
        std::fs::write(&app, "# Initial App Content").unwrap();
        let other = temp.path().join("other.md");
        std::fs::write(&other, "# Other Content").unwrap();

        let mut lane = local_lane("dev-lane", "my-project", temp.path());
        lane.components = vec!["app".into(), "worker".into()];
        let config = Config::default();

        // Baseline: guide is None
        let d0 = config.effective_profile_digest(&lane).unwrap();

        // Guide added
        lane.guide = Some(GuideConfig {
            project: Some(overview.clone()),
            environment: Some(dev.clone()),
            components: BTreeMap::from([("app".into(), app.clone())]),
        });
        let d1 = config.effective_profile_digest(&lane).unwrap();
        assert_eq!(d0, d1, "adding guide must not change digest");

        // Mutating file content on disk
        std::fs::write(&overview, "# Completely Overhauled Content With More Text").unwrap();
        std::fs::write(&dev, "# Rewritten dev guidelines\n1. Step one\n2. Step two").unwrap();
        let d2 = config.effective_profile_digest(&lane).unwrap();
        assert_eq!(d0, d2, "mutating guide file content must not change digest");

        // Changing guide paths in config
        lane.guide = Some(GuideConfig {
            project: Some(other.clone()),
            environment: Some(other.clone()),
            components: BTreeMap::from([
                ("app".into(), other.clone()),
                ("worker".into(), other.clone()),
            ]),
        });
        let d3 = config.effective_profile_digest(&lane).unwrap();
        assert_eq!(d0, d3, "changing guide file paths must not change digest");

        // Removing guide components
        lane.guide = Some(GuideConfig {
            project: None,
            environment: None,
            components: BTreeMap::new(),
        });
        let d4 = config.effective_profile_digest(&lane).unwrap();
        assert_eq!(d0, d4, "clearing guide fields must not change digest");

        // Returning to None
        lane.guide = None;
        let d5 = config.effective_profile_digest(&lane).unwrap();
        assert_eq!(d0, d5, "returning to guide None must preserve digest");
    }
}
