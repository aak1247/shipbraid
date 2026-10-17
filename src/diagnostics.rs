//! Offline prerequisite checks. This module never opens the coordinator store
//! or invokes a configured hook.

mod configuration;
mod probes;

use std::path::Path;

use serde::Serialize;

use crate::config::Config;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
    Skipped,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub code: &'static str,
    pub status: CheckStatus,
    pub field: String,
    pub message: &'static str,
    pub suggestion: &'static str,
}

impl Check {
    fn new(
        code: &'static str,
        status: CheckStatus,
        field: impl Into<String>,
        message: &'static str,
        suggestion: &'static str,
    ) -> Self {
        Self {
            code,
            status,
            field: field.into(),
            message,
            suggestion,
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub passed: usize,
    pub warnings: usize,
    pub failed: usize,
    pub skipped: usize,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema_version: u8,
    pub version: &'static str,
    pub ok: bool,
    pub checks: Vec<Check>,
    pub summary: Summary,
    pub scope: &'static str,
}

impl Report {
    fn from_checks(checks: Vec<Check>) -> Self {
        let mut summary = Summary::default();
        for check in &checks {
            match check.status {
                CheckStatus::Pass => summary.passed += 1,
                CheckStatus::Warn => summary.warnings += 1,
                CheckStatus::Fail => summary.failed += 1,
                CheckStatus::Skipped => summary.skipped += 1,
            }
        }
        Self {
            schema_version: 1,
            version: env!("CARGO_PKG_VERSION"),
            ok: summary.failed == 0,
            checks,
            summary,
            scope: "Prerequisites only: no deployment, database, hook execution or environment acceptance was performed.",
        }
    }

    pub fn to_text(&self) -> String {
        let mut lines = vec![format!("Shipbraid doctor {}", self.version)];
        for check in &self.checks {
            lines.push(format!(
                "[{}] {} ({}) - {} {}",
                match check.status {
                    CheckStatus::Pass => "PASS",
                    CheckStatus::Warn => "WARN",
                    CheckStatus::Fail => "FAIL",
                    CheckStatus::Skipped => "SKIPPED",
                },
                check.code,
                check.field,
                check.message,
                check.suggestion
            ));
        }
        lines.push(format!(
            "Summary: {} passed, {} warnings, {} failed, {} skipped.",
            self.summary.passed, self.summary.warnings, self.summary.failed, self.summary.skipped
        ));
        lines.push(self.scope.to_owned());
        lines.join("\n")
    }
}

/// Read local configuration and selected host prerequisites without contacting
/// the coordinator, acquiring a database lock, or running project commands.
pub async fn diagnose(path: &Path, check_local_runtime: bool) -> Report {
    let config = match Config::load_unvalidated(path) {
        Ok(config) => config,
        Err(_) => {
            return Report::from_checks(vec![Check::new(
                "config.load",
                CheckStatus::Fail,
                "config",
                "Configuration could not be read or parsed.",
                "Check the supplied file, TOML syntax and required field types. Parser details are withheld to protect secrets.",
            )]);
        }
    };
    let mut checks = vec![if config.validate().is_ok() {
        Check::new(
            "config.validate",
            CheckStatus::Pass,
            "config",
            "Configuration satisfies the coordinator's static rules.",
            "Host prerequisites are checked separately.",
        )
    } else {
        Check::new(
            "config.validate",
            CheckStatus::Fail,
            "config",
            "Configuration does not satisfy the coordinator's static rules.",
            "Review required fields, lane/token scopes and hook settings; specific path and credential checks follow.",
        )
    }];
    checks.extend(configuration::configuration_checks(&config));
    checks.extend(probes::repository_checks(&config).await);
    checks.extend(probes::runtime_checks(check_local_runtime).await);
    Report::from_checks(checks)
}
