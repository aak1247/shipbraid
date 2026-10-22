use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::{RunState, State};

/// Administrator-controlled boundary for resource registration and planning.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResourceRootConfig {
    pub id: String,
    pub path: PathBuf,
    pub managed_budget_bytes: u64,
    #[serde(default)]
    pub minimum_free_bytes: u64,
}

/// Fixed per-lane reservation; request bodies cannot choose a path or size.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResourceAdmissionConfig {
    pub root_id: String,
    pub reservation_bytes: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    BuildCache,
    Sccache,
    TemporaryArtifact,
    Backup,
    RunLog,
    ReleaseArtifact,
    ServiceConfiguration,
    Credential,
    FrozenHook,
    Other,
}

impl ResourceKind {
    pub fn may_be_reclaimed(&self) -> bool {
        matches!(
            self,
            Self::BuildCache | Self::Sccache | Self::TemporaryArtifact
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePinKind {
    ActiveExecution,
    UnknownExecution,
    CurrentRelease,
    RollbackCandidate,
    RecoveryBackup,
    FrozenHook,
    ServiceConfiguration,
    Manual,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResourcePin {
    pub kind: ResourcePinKind,
    pub reference_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResourceRegistration {
    pub root_id: String,
    pub relative_path: String,
    pub owner: String,
    pub project: String,
    pub kind: ResourceKind,
    pub input_digest: String,
    pub toolchain: String,
    pub platform: String,
    pub observed_bytes: u64,
    pub last_used_at: Option<DateTime<Utc>>,
    pub recreatable: bool,
    pub execution_ref: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ManagedResource {
    pub schema_version: u32,
    pub id: String,
    pub root_id: String,
    pub relative_path: String,
    pub canonical_path: String,
    pub owner: String,
    pub project: String,
    pub kind: ResourceKind,
    pub input_digest: String,
    pub toolchain: String,
    pub platform: String,
    pub observed_bytes: u64,
    pub last_used_at: DateTime<Utc>,
    pub recreatable: bool,
    pub execution_ref: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub pins: Vec<ResourcePin>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResourceEvent {
    pub sequence: u64,
    pub resource_id: String,
    pub actor: String,
    pub action: String,
    pub timestamp: DateTime<Utc>,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ResourceGcPlan {
    pub generated_at: DateTime<Utc>,
    pub read_only: bool,
    pub roots: Vec<ResourceRootGcPlan>,
    pub unconfigured_resources: Vec<ResourceGcBlocker>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ResourceRootGcPlan {
    pub root_id: String,
    pub managed_budget_bytes: u64,
    pub minimum_free_bytes: u64,
    pub available_bytes: Option<u64>,
    pub registered_observed_bytes: u64,
    pub active_reservation_bytes: u64,
    pub estimated_reclaim_needed_bytes: u64,
    pub selected: Vec<ResourceGcCandidate>,
    pub eligible: Vec<ResourceGcCandidate>,
    pub blocked: Vec<ResourceGcBlocker>,
    pub estimate_only: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ResourceGcCandidate {
    pub id: String,
    pub canonical_path: String,
    pub kind: ResourceKind,
    pub observed_bytes: u64,
    pub last_used_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ResourceGcBlocker {
    pub id: String,
    pub canonical_path: String,
    pub observed_bytes: u64,
    pub reason: String,
}

/// Treat roots as the same capacity pool when they resolve to the same
/// filesystem. If the platform cannot identify one side, conservatively treat
/// the roots as sharing capacity so an unknown reservation cannot be spent a
/// second time.
pub fn roots_may_share_filesystem(left: &ResourceRootConfig, right: &ResourceRootConfig) -> bool {
    match (filesystem_key(&left.path), filesystem_key(&right.path)) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

fn filesystem_key(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!("device:{}", metadata.dev()))
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        use std::path::Component;
        let canonical = path.canonicalize().ok()?;
        canonical
            .components()
            .find_map(|component| match component {
                Component::Prefix(prefix) => Some(format!(
                    "volume:{}",
                    prefix.as_os_str().to_string_lossy().to_lowercase()
                )),
                _ => None,
            })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        path.canonicalize()
            .ok()?
            .components()
            .next()
            .map(|component| format!("root:{component:?}"))
    }
}

pub fn validate_resource_id(id: &str) -> Result<(), String> {
    let mut bytes = id.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric());
    if !valid_first
        || id.len() > 128
        || !bytes
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
    {
        return Err("resource id must match [A-Za-z0-9][A-Za-z0-9._:-]{0,127}".into());
    }
    Ok(())
}

pub fn resolve_resource_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let path = Path::new(relative);
    if relative.trim().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(
            "resource path must be a nonempty relative path below the configured root".into(),
        );
    }
    let resolved = root
        .join(path)
        .canonicalize()
        .map_err(|_| "resource path must exist within the configured root".to_owned())?;
    if !resolved.starts_with(root) {
        return Err("resource path resolves outside the configured root".into());
    }
    if resolved == root {
        return Err("resource path must identify a resource below the configured root".into());
    }
    Ok(resolved)
}

pub fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

pub fn active_reservations(state: &State, root_id: &str) -> BTreeMap<Uuid, u64> {
    state
        .runs
        .values()
        .filter(|run| run_holds_reservation(state, run.id, &run.state))
        .filter_map(|run| {
            run.resource_reservation
                .as_ref()
                .filter(|reservation| reservation.root_id == root_id)
                .map(|reservation| (run.id, reservation.bytes))
        })
        .collect()
}

pub fn active_reservations_on_filesystem(
    state: &State,
    roots: &[ResourceRootConfig],
    target_root_id: &str,
) -> BTreeMap<Uuid, u64> {
    let Some(target_root) = roots.iter().find(|root| root.id == target_root_id) else {
        return BTreeMap::new();
    };
    let root_ids = roots
        .iter()
        .filter(|root| roots_may_share_filesystem(target_root, root))
        .map(|root| root.id.as_str())
        .collect::<BTreeSet<_>>();
    state
        .runs
        .values()
        .filter(|run| run_holds_reservation(state, run.id, &run.state))
        .filter_map(|run| {
            run.resource_reservation
                .as_ref()
                .filter(|reservation| root_ids.contains(reservation.root_id.as_str()))
                .map(|reservation| (run.id, reservation.bytes))
        })
        .collect()
}

pub fn roots_on_filesystem<'a>(
    roots: &'a [ResourceRootConfig],
    target_root_id: &str,
) -> Vec<&'a ResourceRootConfig> {
    let Some(target_root) = roots.iter().find(|root| root.id == target_root_id) else {
        return Vec::new();
    };
    roots
        .iter()
        .filter(|root| roots_may_share_filesystem(target_root, root))
        .collect()
}

pub fn run_holds_reservation(state: &State, run_id: Uuid, run_state: &RunState) -> bool {
    if run_state.owns_execution_slot() {
        return true;
    }
    if *run_state != RunState::Planned {
        return false;
    }
    state
        .runs
        .values()
        .any(|owner| owner.replacement_run_id == Some(run_id) && owner.state.owns_execution_slot())
}

pub fn build_gc_plan(
    resources: &[ManagedResource],
    state: &State,
    roots: &[ResourceRootConfig],
    available_by_root: &BTreeMap<String, Option<u64>>,
    now: DateTime<Utc>,
) -> ResourceGcPlan {
    let mut plans = Vec::with_capacity(roots.len());
    let configured_root_ids = roots
        .iter()
        .map(|root| root.id.as_str())
        .collect::<BTreeSet<_>>();
    let unconfigured_resources = resources
        .iter()
        .filter(|resource| !configured_root_ids.contains(resource.root_id.as_str()))
        .map(|resource| ResourceGcBlocker {
            id: resource.id.clone(),
            canonical_path: resource.canonical_path.clone(),
            observed_bytes: resource.observed_bytes,
            reason: "resource_root_not_configured".into(),
        })
        .collect();
    for root in roots {
        let root_resources = resources
            .iter()
            .filter(|resource| resource.root_id == root.id)
            .collect::<Vec<_>>();
        let filesystem_roots = roots_on_filesystem(roots, &root.id);
        let available = filesystem_roots
            .iter()
            .map(|filesystem_root| {
                available_by_root
                    .get(&filesystem_root.id)
                    .copied()
                    .flatten()
            })
            .try_fold(u64::MAX, |minimum, value| {
                value.map(|value| minimum.min(value))
            });
        let available = available.filter(|_| !filesystem_roots.is_empty());
        let reservations = active_reservations_on_filesystem(state, roots, &root.id);
        let active_reservation_bytes = reservations
            .values()
            .copied()
            .fold(0_u64, u64::saturating_add);
        let registered_observed_bytes = root_resources
            .iter()
            .map(|resource| resource.observed_bytes)
            .fold(0_u64, u64::saturating_add);
        let over_budget = registered_observed_bytes.saturating_sub(root.managed_budget_bytes);
        let filesystem_minimum_free_bytes = filesystem_roots
            .iter()
            .map(|filesystem_root| filesystem_root.minimum_free_bytes)
            .max()
            .unwrap_or(root.minimum_free_bytes);
        let free_shortfall = available
            .map(|bytes| {
                filesystem_minimum_free_bytes
                    .saturating_add(active_reservation_bytes)
                    .saturating_sub(bytes)
            })
            .unwrap_or(filesystem_minimum_free_bytes.saturating_add(active_reservation_bytes));
        let estimated_reclaim_needed_bytes = over_budget.max(free_shortfall);

        let mut eligible = Vec::new();
        let mut blocked = Vec::new();
        for resource in &root_resources {
            let reason = if active_reservation_bytes > 0 {
                Some("active_or_unknown_execution_reservation")
            } else if Path::new(&resource.canonical_path)
                .canonicalize()
                .map_or(true, |current| {
                    current.as_path() != Path::new(&resource.canonical_path)
                        || !current.starts_with(&root.path)
                })
            {
                Some("resource_missing_or_path_changed")
            } else if !resource.kind.may_be_reclaimed() || !resource.recreatable {
                Some("resource_is_not_declared_recreatable_cache")
            } else if !resource.pins.is_empty() {
                Some("resource_has_retention_pins")
            } else if let Some(reference) = &resource.execution_ref {
                match Uuid::parse_str(reference)
                    .ok()
                    .and_then(|id| state.runs.get(&id))
                {
                    Some(run) if run.state.owns_execution_slot() => {
                        Some("execution_active_or_unknown")
                    }
                    Some(_) => None,
                    None => Some("execution_reference_unresolved"),
                }
            } else {
                None
            };
            let candidate = ResourceGcCandidate {
                id: resource.id.clone(),
                canonical_path: resource.canonical_path.clone(),
                kind: resource.kind,
                observed_bytes: resource.observed_bytes,
                last_used_at: resource.last_used_at,
            };
            if let Some(reason) = reason {
                blocked.push(ResourceGcBlocker {
                    id: resource.id.clone(),
                    canonical_path: resource.canonical_path.clone(),
                    observed_bytes: resource.observed_bytes,
                    reason: reason.into(),
                });
            } else {
                eligible.push(candidate);
            }
        }
        eligible.sort_by_key(|candidate| (candidate.last_used_at, candidate.id.clone()));
        let mut estimated_selected = 0_u64;
        let selected = if active_reservation_bytes > 0 || available.is_none() {
            if available.is_none() {
                for candidate in eligible.drain(..) {
                    blocked.push(ResourceGcBlocker {
                        id: candidate.id,
                        canonical_path: candidate.canonical_path,
                        observed_bytes: candidate.observed_bytes,
                        reason: "filesystem_capacity_unavailable".into(),
                    });
                }
            }
            Vec::new()
        } else {
            eligible
                .iter()
                .take_while(|candidate| {
                    let include = estimated_selected < estimated_reclaim_needed_bytes;
                    if include {
                        estimated_selected =
                            estimated_selected.saturating_add(candidate.observed_bytes);
                    }
                    include
                })
                .cloned()
                .collect()
        };
        plans.push(ResourceRootGcPlan {
            root_id: root.id.clone(),
            managed_budget_bytes: root.managed_budget_bytes,
            minimum_free_bytes: filesystem_minimum_free_bytes,
            available_bytes: available,
            registered_observed_bytes,
            active_reservation_bytes,
            estimated_reclaim_needed_bytes,
            selected,
            eligible,
            blocked,
            estimate_only: true,
        });
    }
    ResourceGcPlan {
        generated_at: now,
        read_only: true,
        roots: plans,
        unconfigured_resources,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CoverageMode, DeliveryRun, Phase, Priority, ResourceReservation, Target};
    use chrono::TimeZone;
    use std::collections::{BTreeMap, BTreeSet};

    fn root(id: &str, path: PathBuf, budget: u64, minimum_free: u64) -> ResourceRootConfig {
        ResourceRootConfig {
            id: id.into(),
            path,
            managed_budget_bytes: budget,
            minimum_free_bytes: minimum_free,
        }
    }

    fn reservation_run(root_id: &str, bytes: u64, state: RunState) -> DeliveryRun {
        let now = Utc::now();
        DeliveryRun {
            id: Uuid::new_v4(),
            lane_id: "lane".into(),
            target: Target {
                repository_id: "repo".into(),
                revision: "a".repeat(40),
                components: BTreeSet::from(["app".into()]),
                profile_digest: "profile".into(),
                release_line_id: "line".into(),
                config_version: 1,
                coverage_mode: CoverageMode::Exact,
                parameters: BTreeMap::new(),
            },
            effective_priority: Priority::Normal,
            state,
            phase: Phase::Building,
            generation: 1,
            executor_ref: None,
            replacement_run_id: None,
            cancel_reason: None,
            receipt_id: None,
            resource_reservation: Some(ResourceReservation {
                root_id: root_id.into(),
                bytes,
            }),
            created_at: now,
            phase_started_at: now,
            updated_at: now,
        }
    }

    fn managed_resource(root_id: &str, id: &str, bytes: u64, age_seconds: i64) -> ManagedResource {
        let last_used_at = Utc::now() - chrono::Duration::seconds(age_seconds);
        ManagedResource {
            schema_version: 1,
            id: id.into(),
            root_id: root_id.into(),
            relative_path: id.into(),
            canonical_path: format!("/tmp/{id}"),
            owner: "autotask".into(),
            project: "project".into(),
            kind: ResourceKind::BuildCache,
            input_digest: "digest".into(),
            toolchain: "rustc".into(),
            platform: "linux".into(),
            observed_bytes: bytes,
            last_used_at,
            recreatable: true,
            execution_ref: None,
            created_at: last_used_at,
            updated_at: last_used_at,
            pins: Vec::new(),
        }
    }

    #[test]
    fn resource_ids_are_single_safe_ascii_path_segments() {
        for id in ["cache", "autotask-brotli-v1", "v1.entries:release"] {
            assert!(validate_resource_id(id).is_ok(), "{id}");
        }
        for id in ["", "../cache", "cache/path", "with space", "caché", "#tag"] {
            assert!(validate_resource_id(id).is_err(), "{id}");
        }
        assert!(validate_resource_id(&format!("r{}", "x".repeat(128))).is_err());
    }

    #[test]
    fn reservations_and_free_space_are_aggregated_per_filesystem() {
        let temp = tempfile::tempdir().unwrap();
        let roots = vec![
            root("cache", temp.path().to_path_buf(), 100, 10),
            root("build", temp.path().to_path_buf(), 200, 40),
        ];
        assert!(roots_may_share_filesystem(&roots[0], &roots[1]));

        let mut state = State::default();
        let active = reservation_run("cache", 12, RunState::Running);
        let unknown = reservation_run("build", 30, RunState::Unknown);
        state.runs.insert(active.id, active);
        state.runs.insert(unknown.id, unknown);
        let reservations = active_reservations_on_filesystem(&state, &roots, "cache");
        assert_eq!(reservations.values().copied().sum::<u64>(), 42);

        let plan = build_gc_plan(
            &[managed_resource("cache", "cache-one", 150, 3600)],
            &state,
            &roots,
            &BTreeMap::from([("cache".into(), Some(128)), ("build".into(), Some(96))]),
            Utc.timestamp_opt(1_800_000_000, 0).unwrap(),
        );
        assert_eq!(plan.roots[0].active_reservation_bytes, 42);
        assert_eq!(plan.roots[0].available_bytes, Some(96));
        assert_eq!(plan.roots[0].minimum_free_bytes, 40);
        assert_eq!(plan.roots[0].selected.len(), 0);
        assert_eq!(
            plan.roots[0].blocked[0].reason,
            "active_or_unknown_execution_reservation"
        );
        assert!(plan.read_only);
    }

    #[test]
    fn gc_plan_selects_only_old_recreatable_unpinned_resources_and_keeps_unknown_roots_visible() {
        let temp = tempfile::tempdir().unwrap();
        let cache_dir = temp.path().join("cache");
        std::fs::create_dir(&cache_dir).unwrap();
        let fresh_path = cache_dir.join("fresh");
        let old_path = cache_dir.join("old");
        let pinned_path = cache_dir.join("pinned");
        std::fs::create_dir(&fresh_path).unwrap();
        std::fs::create_dir(&old_path).unwrap();
        std::fs::create_dir(&pinned_path).unwrap();
        let root = root("cache", cache_dir.clone(), 100, 10);
        let mut old = managed_resource("cache", "old", 80, 3600);
        old.canonical_path = old_path.to_string_lossy().into_owned();
        let mut fresh = managed_resource("cache", "fresh", 50, 60);
        fresh.canonical_path = fresh_path.to_string_lossy().into_owned();
        let mut pinned = managed_resource("cache", "pinned", 20, 7200);
        pinned.canonical_path = pinned_path.to_string_lossy().into_owned();
        pinned.pins.push(ResourcePin {
            kind: ResourcePinKind::CurrentRelease,
            reference_id: "release-sha".into(),
        });
        let orphan = managed_resource("removed-root", "orphan", 200, 7200);
        let plan = build_gc_plan(
            &[fresh, old, pinned, orphan.clone()],
            &State::default(),
            std::slice::from_ref(&root),
            &BTreeMap::from([("cache".into(), Some(0))]),
            Utc::now(),
        );
        assert_eq!(plan.roots[0].selected.len(), 1);
        assert_eq!(plan.roots[0].selected[0].id, "old");
        assert_eq!(plan.roots[0].blocked[0].id, "pinned");
        assert_eq!(plan.unconfigured_resources.len(), 1);
        assert_eq!(plan.unconfigured_resources[0].id, orphan.id);
        assert!(old_path.exists() && fresh_path.exists() && pinned_path.exists());
    }
}
