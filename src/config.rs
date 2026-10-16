use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub database_path: PathBuf,
    pub listen: SocketAddr,
    pub tokens: Vec<TokenConfig>,
    pub lanes: Vec<LaneConfig>,
    pub max_active_runs: usize,
    pub tick_ms: u64,
    pub scripted_phase_ms: u64,
    pub cancel_timeout_seconds: u64,
    pub coalesce_ms: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            database_path: "./data/auto-deliver.db".into(),
            listen: "127.0.0.1:8787".parse().unwrap(),
            tokens: vec![],
            lanes: vec![],
            max_active_runs: 2,
            tick_ms: 250,
            scripted_phase_ms: 3000,
            cancel_timeout_seconds: 60,
            coalesce_ms: 2000,
        }
    }
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
    pub allow_descendant: bool,
    #[serde(default)]
    pub allow_preemption: bool,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}
fn default_enabled() -> bool {
    true
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
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
        for lane in &mut config.lanes {
            if lane.repository_path.is_relative() {
                lane.repository_path = base.join(&lane.repository_path);
            }
        }
        config.validate()?;
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
        if self.tokens.is_empty() {
            bail!("at least one authentication token must be configured");
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
}

#[cfg(test)]
mod tests {
    use super::*;
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
    }
}
