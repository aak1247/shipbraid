use std::{collections::BTreeMap, path::Path, time::Duration};

use anyhow::{Context, bail};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlx::{
    Row,
    migrate::Migrator,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::domain::{LOCAL_TENANT_ID, State};
use crate::resources::{ManagedResource, ResourceEvent, ResourcePin, ResourcePinKind};

#[derive(Debug, thiserror::Error)]
pub enum ResourceStoreError {
    #[error("resource was not found")]
    NotFound,
    #[error("resource conflict: {0}")]
    Conflict(String),
    #[error("resource storage operation failed: {0}")]
    Storage(#[source] anyhow::Error),
}

impl From<sqlx::Error> for ResourceStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Storage(error.into())
    }
}

impl From<serde_json::Error> for ResourceStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Storage(error.into())
    }
}

const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
// Opening a connection may include filesystem initialization under host load.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(15);
static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

#[derive(Clone)]
pub struct Store {
    pool: sqlx::SqlitePool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlitePragmas {
    pub version: String,
    pub foreign_keys: bool,
    pub synchronous_full: bool,
    pub wal: bool,
}

impl Store {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create database directory {}", parent.display()))?;
        }

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(WRITE_TIMEOUT);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect_with(options)
            .await
            .with_context(|| format!("open SQLite database {}", path.display()))?;

        MIGRATOR
            .run(&pool)
            .await
            .context("apply SQLite migrations")?;

        let pragmas = Self::pragma_status_for(&pool).await?;
        if !pragmas.foreign_keys || !pragmas.synchronous_full || !pragmas.wal {
            bail!(
                "SQLite safety pragmas are not active (foreign_keys={}, synchronous_full={}, wal={})",
                pragmas.foreign_keys,
                pragmas.synchronous_full,
                pragmas.wal
            );
        }
        if sqlite_version_tuple(&pragmas.version).is_none_or(|version| version < (3, 51, 3)) {
            bail!(
                "SQLite {} is too old; version 3.51.3 or newer is required for WAL safety",
                pragmas.version
            );
        }
        Ok(Self { pool })
    }

    async fn pragma_status_for(pool: &sqlx::SqlitePool) -> anyhow::Result<SqlitePragmas> {
        let version: String = sqlx::query_scalar("SELECT sqlite_version()")
            .fetch_one(pool)
            .await?;
        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(pool)
            .await?;
        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(pool)
            .await?;
        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(pool)
            .await?;
        Ok(SqlitePragmas {
            version,
            foreign_keys: foreign_keys == 1,
            synchronous_full: synchronous == 2,
            wal: journal_mode.eq_ignore_ascii_case("wal"),
        })
    }

    pub async fn pragma_status(&self) -> anyhow::Result<SqlitePragmas> {
        Self::pragma_status_for(&self.pool).await
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        sqlx::query_scalar::<_, i64>("SELECT 1")
            .fetch_one(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn load_state(&self) -> anyhow::Result<State> {
        let mut root = serde_json::to_value(State::default())?;
        let object = root
            .as_object_mut()
            .context("domain State must serialize to an object")?;

        object.insert(
            "lanes".into(),
            self.read_map("SELECT id, tenant_id, entity_json FROM lanes ORDER BY id")
                .await?,
        );
        object.insert(
            "requests".into(),
            self.read_map("SELECT id, tenant_id, entity_json FROM requests ORDER BY id")
                .await?,
        );
        object.insert(
            "runs".into(),
            self.read_map("SELECT id, tenant_id, entity_json FROM runs ORDER BY id")
                .await?,
        );
        object.insert(
            "receipts".into(),
            self.read_map("SELECT id, tenant_id, entity_json FROM receipts ORDER BY id")
                .await?,
        );
        object.insert(
            "commands".into(),
            self.read_map("SELECT id, tenant_id, entity_json FROM commands ORDER BY id")
                .await?,
        );
        object.insert("events".into(), self.read_events().await?);
        object.insert(
            "next_event_sequence".into(),
            self.read_event_sequences().await?,
        );
        if let Some(draining) =
            sqlx::query_scalar::<_, String>("SELECT value FROM runtime_meta WHERE key = 'draining'")
                .fetch_optional(&self.pool)
                .await?
        {
            object.insert("draining".into(), Value::Bool(draining == "true"));
        }

        serde_json::from_value(root).context("decode persistent domain state")
    }

    /// Persists the changed rows and appended events atomically. Entity deletes
    /// and event edits are rejected because the M1 ledger is append-only.
    /// Callers publish `next` only after this transaction commits.
    pub async fn commit_transition(&self, previous: &State, next: &State) -> anyhow::Result<()> {
        for state in [previous, next] {
            if state
                .lanes
                .values()
                .any(|lane| lane.tenant_id != LOCAL_TENANT_ID)
                || state
                    .requests
                    .values()
                    .any(|request| request.tenant_id != LOCAL_TENANT_ID)
            {
                bail!("only the local tenant is supported");
            }
        }
        // Compare typed rows first. A normal transition usually changes only
        // a handful of records, so avoid serializing the entire event/entity
        // history just to discover those changes.
        let changed_lanes = changed_entries(&previous.lanes, &next.lanes, "lanes")?;
        let changed_runs = changed_entries(&previous.runs, &next.runs, "runs")?;
        let changed_requests = changed_entries(&previous.requests, &next.requests, "requests")?;
        let changed_receipts = changed_entries(&previous.receipts, &next.receipts, "receipts")?;
        let changed_commands = changed_entries(&previous.commands, &next.commands, "commands")?;
        if next.events.len() < previous.events.len()
            || next.events[..previous.events.len()] != previous.events[..]
        {
            bail!("event history must remain append-only");
        }
        let appended_events = next.events[previous.events.len()..]
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?;
        let draining_changed = previous.draining != next.draining;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .context("begin immediate state transaction")?;

        for (id, value) in &changed_lanes {
            let entity = encode(value)?;
            let updated_at = string_field(value, "updated_at")
                .or_else(|| string_field(value, "created_at"))
                .unwrap_or("1970-01-01T00:00:00Z");
            sqlx::query(
                "INSERT INTO lanes (id, project, environment, repository_id, release_line_id, release_ref, config_version, profile_digest, active_run_id, generation, blocked_reason, enabled, failure_paused, entity_json, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(id) DO UPDATE SET project=excluded.project, environment=excluded.environment, repository_id=excluded.repository_id, release_line_id=excluded.release_line_id, release_ref=excluded.release_ref, config_version=excluded.config_version, profile_digest=excluded.profile_digest, active_run_id=excluded.active_run_id, generation=excluded.generation, blocked_reason=excluded.blocked_reason, enabled=excluded.enabled, failure_paused=excluded.failure_paused, entity_json=excluded.entity_json, updated_at=excluded.updated_at \
                 WHERE lanes.entity_json <> excluded.entity_json",
            )
            .bind(id)
            .bind(string_field(value, "project").unwrap_or_default())
            .bind(string_field(value, "environment").unwrap_or_default())
            .bind(string_field(value, "repository_id").unwrap_or_default())
            .bind(string_field(value, "release_line_id").unwrap_or_default())
            .bind(string_field(value, "release_ref").unwrap_or_default())
            .bind(unsigned_field(value, "config_version"))
            .bind(string_field(value, "profile_digest").unwrap_or_default())
            .bind(optional_id_field(value, "active_run_id"))
            .bind(unsigned_field(value, "generation"))
            .bind(optional_string_field(value, "blocked_reason"))
            .bind(bool_field_value(value, "enabled"))
            .bind(bool_field_value(value, "failure_paused"))
            .bind(entity)
            .bind(updated_at)
            .execute(&mut *tx)
            .await?;
        }

        for (id, value) in &changed_runs {
            let entity = encode(value)?;
            let run_id = string_field(value, "id").unwrap_or(id.as_str());
            sqlx::query(
                "INSERT INTO runs (id, lane_id, generation, state, phase, replacement_run_id, created_at, phase_started_at, updated_at, entity_json) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(id) DO UPDATE SET lane_id=excluded.lane_id, generation=excluded.generation, state=excluded.state, phase=excluded.phase, replacement_run_id=excluded.replacement_run_id, phase_started_at=excluded.phase_started_at, updated_at=excluded.updated_at, entity_json=excluded.entity_json \
                 WHERE runs.entity_json <> excluded.entity_json",
            )
            .bind(run_id)
            .bind(string_field(value, "lane_id").unwrap_or_default())
            .bind(unsigned_field(value, "generation"))
            .bind(enum_field(value, "state"))
            .bind(enum_field(value, "phase"))
            .bind(optional_id_field(value, "replacement_run_id"))
            .bind(string_field(value, "created_at").unwrap_or_default())
            .bind(string_field(value, "phase_started_at").unwrap_or_default())
            .bind(string_field(value, "updated_at").unwrap_or_default())
            .bind(entity)
            .execute(&mut *tx)
            .await?;
        }

        for (id, value) in &changed_requests {
            let entity = encode(value)?;
            let request_id = string_field(value, "id").unwrap_or(id.as_str());
            sqlx::query(
                "INSERT INTO requests (id, lane_id, principal, idempotency_key, request_hash, status, current_run_id, satisfied_by_run, waiting_reason, created_at, entity_json, task_id) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(id) DO UPDATE SET lane_id=excluded.lane_id, principal=excluded.principal, idempotency_key=excluded.idempotency_key, request_hash=excluded.request_hash, status=excluded.status, current_run_id=excluded.current_run_id, satisfied_by_run=excluded.satisfied_by_run, waiting_reason=excluded.waiting_reason, entity_json=excluded.entity_json, task_id=excluded.task_id \
                 WHERE requests.entity_json <> excluded.entity_json",
            )
            .bind(request_id)
            .bind(string_field(value, "lane_id").unwrap_or_default())
            .bind(string_field(value, "principal").unwrap_or_default())
            .bind(string_field(value, "idempotency_key").unwrap_or_default())
            .bind(string_field(value, "request_hash").unwrap_or_default())
            .bind(enum_field(value, "status"))
            .bind(optional_id_field(value, "current_run_id"))
            .bind(optional_id_field(value, "satisfied_by_run"))
            .bind(optional_string_field(value, "waiting_reason"))
            .bind(string_field(value, "created_at").unwrap_or_default())
            .bind(entity)
            .bind(value.get("context").and_then(|context| string_field(context, "task_id")))
            .execute(&mut *tx)
            .await?;
        }

        for (id, value) in &changed_receipts {
            let entity = encode(value)?;
            let receipt_id = string_field(value, "id").unwrap_or(id.as_str());
            sqlx::query(
                "INSERT INTO receipts (id, run_id, observed_at, result, entity_json) VALUES (?, ?, ?, ?, ?) \
                 ON CONFLICT(id) DO UPDATE SET run_id=excluded.run_id, observed_at=excluded.observed_at, result=excluded.result, entity_json=excluded.entity_json \
                 WHERE receipts.entity_json <> excluded.entity_json",
            )
            .bind(receipt_id)
            .bind(string_field(value, "run_id").unwrap_or_default())
            .bind(string_field(value, "observed_at").unwrap_or_default())
            .bind(enum_or_json_field(value, "result"))
            .bind(entity)
            .execute(&mut *tx)
            .await?;
        }

        for (id, value) in &changed_commands {
            let entity = encode(value)?;
            let command_id = string_field(value, "id").unwrap_or(id.as_str());
            sqlx::query(
                "INSERT INTO commands (id, run_id, generation, kind, delivery_state, payload_digest, created_at, entity_json) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation, kind=excluded.kind, delivery_state=excluded.delivery_state, payload_digest=excluded.payload_digest, entity_json=excluded.entity_json \
                 WHERE commands.entity_json <> excluded.entity_json",
            )
            .bind(command_id)
            .bind(string_field(value, "run_id").unwrap_or_default())
            .bind(unsigned_field(value, "generation"))
            .bind(enum_field(value, "kind"))
            .bind(enum_field(value, "delivery_state"))
            .bind(string_field(value, "payload_digest").unwrap_or_default())
            .bind(string_field(value, "created_at").unwrap_or_default())
            .bind(entity)
            .execute(&mut *tx)
            .await?;
        }

        for value in &appended_events {
            let lane_id = string_field(value, "lane_id").unwrap_or_default();
            let sequence = unsigned_field(value, "sequence");
            let event_id = format!("{lane_id}:{sequence}");
            let entity = encode(value)?;
            sqlx::query(
                "INSERT INTO events (id, lane_id, sequence, run_id, request_id, timestamp, kind, message, entity_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(event_id)
            .bind(lane_id)
            .bind(sequence)
            .bind(optional_id_field(value, "run_id"))
            .bind(optional_id_field(value, "request_id"))
            .bind(string_field(value, "timestamp").unwrap_or_default())
            .bind(string_field(value, "kind").unwrap_or_default())
            .bind(string_field(value, "message").unwrap_or_default())
            .bind(entity)
            .execute(&mut *tx)
            .await?;
        }

        if draining_changed {
            sqlx::query(
                "INSERT INTO runtime_meta(key, value) VALUES ('draining', ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            )
            .bind(if next.draining { "true" } else { "false" })
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await.context("commit state transaction")
    }

    /// Insert or refresh one registered resource. Paths and overlaps are
    /// checked again under the SQLite write lock so concurrent administrators
    /// cannot account for the same bytes twice.
    pub async fn register_resource(
        &self,
        mut resource: ManagedResource,
        actor: &str,
    ) -> Result<ManagedResource, ResourceStoreError> {
        resource.pins.clear();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing = sqlx::query_scalar::<_, String>(
            "SELECT entity_json FROM managed_resources WHERE id = ?",
        )
        .bind(&resource.id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(existing) = &existing {
            let old: ManagedResource = serde_json::from_str(existing)?;
            if old.root_id != resource.root_id || old.canonical_path != resource.canonical_path {
                return Err(ResourceStoreError::Conflict(
                    "a resource id cannot be moved to another root or path".into(),
                ));
            }
        }
        let managed_paths = sqlx::query(
            "SELECT id, canonical_path FROM managed_resources WHERE root_id = ? ORDER BY id",
        )
        .bind(&resource.root_id)
        .fetch_all(&mut *tx)
        .await?;
        let new_path = Path::new(&resource.canonical_path);
        for row in managed_paths {
            let id: String = row.try_get("id")?;
            let path: String = row.try_get("canonical_path")?;
            if id != resource.id && crate::resources::paths_overlap(new_path, Path::new(&path)) {
                return Err(ResourceStoreError::Conflict(format!(
                    "resource path overlaps already registered resource {id}"
                )));
            }
        }

        let is_new = existing.is_none();
        if let Some(existing) = existing {
            let old: ManagedResource = serde_json::from_str(&existing)?;
            resource.created_at = old.created_at;
        }
        let entity = serde_json::to_string(&resource)?;
        let observed_bytes = i64::try_from(resource.observed_bytes).map_err(|_| {
            ResourceStoreError::Conflict("observed_bytes exceeds SQLite's supported range".into())
        })?;
        let kind = resource_kind_name(resource.kind);
        sqlx::query(
            "INSERT INTO managed_resources (id, root_id, canonical_path, owner, project, kind, observed_bytes, last_used_at, recreatable, entity_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET owner=excluded.owner, project=excluded.project, kind=excluded.kind, observed_bytes=excluded.observed_bytes, last_used_at=excluded.last_used_at, recreatable=excluded.recreatable, entity_json=excluded.entity_json",
        )
        .bind(&resource.id)
        .bind(&resource.root_id)
        .bind(&resource.canonical_path)
        .bind(&resource.owner)
        .bind(&resource.project)
        .bind(kind)
        .bind(observed_bytes)
        .bind(resource.last_used_at.to_rfc3339())
        .bind(resource.recreatable)
        .bind(entity)
        .execute(&mut *tx)
        .await?;
        let detail = serde_json::to_string(&resource)?;
        append_resource_event(
            &mut tx,
            &resource.id,
            actor,
            if is_new { "registered" } else { "refreshed" },
            resource.updated_at,
            &detail,
        )
        .await?;
        tx.commit().await?;
        self.get_resource(&resource.id)
            .await
            .map_err(ResourceStoreError::Storage)
    }

    pub async fn list_resources(&self) -> anyhow::Result<Vec<ManagedResource>> {
        let rows = sqlx::query(
            "SELECT entity_json FROM managed_resources ORDER BY root_id, last_used_at, id",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut resources = Vec::with_capacity(rows.len());
        for row in rows {
            let value: String = row.try_get("entity_json")?;
            let mut resource: ManagedResource = serde_json::from_str(&value)?;
            resource.pins = self.resource_pins(&resource.id).await?;
            resources.push(resource);
        }
        Ok(resources)
    }

    pub async fn get_resource(&self, id: &str) -> anyhow::Result<ManagedResource> {
        let value = sqlx::query_scalar::<_, String>(
            "SELECT entity_json FROM managed_resources WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("resource not found: {id}"))?;
        let mut resource: ManagedResource = serde_json::from_str(&value)?;
        resource.pins = self.resource_pins(id).await?;
        Ok(resource)
    }

    pub async fn set_resource_pin(
        &self,
        id: &str,
        pin: &ResourcePin,
        present: bool,
        actor: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<ManagedResource, ResourceStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let exists: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM managed_resources WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if exists.is_none() {
            return Err(ResourceStoreError::NotFound);
        }
        let kind = resource_pin_kind_name(pin.kind);
        let changed = if present {
            sqlx::query(
                "INSERT OR IGNORE INTO resource_pins(resource_id, kind, reference_id, created_at) VALUES (?, ?, ?, ?)",
            )
            .bind(id)
            .bind(kind)
            .bind(&pin.reference_id)
            .bind(now.to_rfc3339())
            .execute(&mut *tx)
            .await?
            .rows_affected()
                > 0
        } else {
            sqlx::query(
                "DELETE FROM resource_pins WHERE resource_id = ? AND kind = ? AND reference_id = ?",
            )
            .bind(id)
            .bind(kind)
            .bind(&pin.reference_id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
                > 0
        };
        if changed {
            let detail = serde_json::to_string(pin)?;
            append_resource_event(
                &mut tx,
                id,
                actor,
                if present { "pin_added" } else { "pin_removed" },
                now,
                &detail,
            )
            .await?;
        }
        tx.commit().await?;
        self.get_resource(id)
            .await
            .map_err(ResourceStoreError::Storage)
    }

    pub async fn list_resource_events(
        &self,
        id: &str,
        after: u64,
        limit: usize,
    ) -> anyhow::Result<Vec<ResourceEvent>> {
        let rows = sqlx::query(
            "SELECT sequence, resource_id, actor, action, timestamp, detail FROM resource_events WHERE resource_id = ? AND sequence > ? ORDER BY sequence LIMIT ?",
        )
        .bind(id)
        .bind(i64::try_from(after).unwrap_or(i64::MAX))
        .bind(i64::try_from(limit).unwrap_or(200))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let sequence: i64 = row.try_get("sequence")?;
                let sequence = u64::try_from(sequence)
                    .map_err(|error| ResourceStoreError::Storage(error.into()))?;
                let timestamp: String = row.try_get("timestamp")?;
                Ok(ResourceEvent {
                    sequence,
                    resource_id: row.try_get("resource_id")?,
                    actor: row.try_get("actor")?,
                    action: row.try_get("action")?,
                    timestamp: chrono::DateTime::parse_from_rfc3339(&timestamp)
                        .map_err(|error| ResourceStoreError::Storage(error.into()))?
                        .with_timezone(&chrono::Utc),
                    detail: row.try_get("detail")?,
                })
            })
            .collect()
    }

    async fn resource_pins(&self, id: &str) -> anyhow::Result<Vec<ResourcePin>> {
        let rows = sqlx::query(
            "SELECT kind, reference_id FROM resource_pins WHERE resource_id = ? ORDER BY kind, reference_id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let kind: String = row.try_get("kind")?;
                Ok(ResourcePin {
                    kind: serde_json::from_value(serde_json::Value::String(kind))?,
                    reference_id: row.try_get("reference_id")?,
                })
            })
            .collect()
    }

    async fn read_map(&self, query: &'static str) -> anyhow::Result<Value> {
        let rows = sqlx::query(query).fetch_all(&self.pool).await?;
        let mut entities = Map::new();
        for row in rows {
            let tenant: String = row.try_get("tenant_id")?;
            let id: String = row.try_get("id")?;
            let value: String = row.try_get("entity_json")?;
            let value = serde_json::from_str(&value)?;
            validate_stored_tenant(&tenant, &value)?;
            entities.insert(id, value);
        }
        Ok(Value::Object(entities))
    }

    async fn read_events(&self) -> anyhow::Result<Value> {
        let rows =
            sqlx::query("SELECT tenant_id, entity_json FROM events ORDER BY lane_id, sequence")
                .fetch_all(&self.pool)
                .await?;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let tenant: String = row.try_get("tenant_id")?;
            let value: String = row.try_get("entity_json")?;
            let value = serde_json::from_str(&value)?;
            validate_stored_tenant(&tenant, &value)?;
            events.push(value);
        }
        Ok(Value::Array(events))
    }

    async fn read_event_sequences(&self) -> anyhow::Result<Value> {
        let rows =
            sqlx::query("SELECT lane_id, MAX(sequence) AS sequence FROM events GROUP BY lane_id")
                .fetch_all(&self.pool)
                .await?;
        let mut sequences = Map::new();
        for row in rows {
            let lane_id: String = row.try_get("lane_id")?;
            let sequence: i64 = row.try_get("sequence")?;
            let next = u64::try_from(sequence)
                .context("stored event sequence must be nonnegative")?
                .checked_add(1)
                .context("event sequence is exhausted")?;
            sequences.insert(lane_id, json!(next));
        }
        Ok(Value::Object(sequences))
    }
}

async fn append_resource_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    resource_id: &str,
    actor: &str,
    action: &str,
    timestamp: chrono::DateTime<chrono::Utc>,
    detail: &str,
) -> Result<(), ResourceStoreError> {
    sqlx::query(
        "INSERT INTO resource_events(resource_id, actor, action, timestamp, detail) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(resource_id)
    .bind(actor)
    .bind(action)
    .bind(timestamp.to_rfc3339())
    .bind(detail)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn resource_kind_name(kind: crate::resources::ResourceKind) -> &'static str {
    match kind {
        crate::resources::ResourceKind::BuildCache => "build_cache",
        crate::resources::ResourceKind::Sccache => "sccache",
        crate::resources::ResourceKind::TemporaryArtifact => "temporary_artifact",
        crate::resources::ResourceKind::Backup => "backup",
        crate::resources::ResourceKind::RunLog => "run_log",
        crate::resources::ResourceKind::ReleaseArtifact => "release_artifact",
        crate::resources::ResourceKind::ServiceConfiguration => "service_configuration",
        crate::resources::ResourceKind::Credential => "credential",
        crate::resources::ResourceKind::FrozenHook => "frozen_hook",
        crate::resources::ResourceKind::Other => "other",
    }
}

fn resource_pin_kind_name(kind: ResourcePinKind) -> &'static str {
    match kind {
        ResourcePinKind::ActiveExecution => "active_execution",
        ResourcePinKind::UnknownExecution => "unknown_execution",
        ResourcePinKind::CurrentRelease => "current_release",
        ResourcePinKind::RollbackCandidate => "rollback_candidate",
        ResourcePinKind::RecoveryBackup => "recovery_backup",
        ResourcePinKind::FrozenHook => "frozen_hook",
        ResourcePinKind::ServiceConfiguration => "service_configuration",
        ResourcePinKind::Manual => "manual",
    }
}

fn validate_stored_tenant(column: &str, entity: &Value) -> anyhow::Result<()> {
    if column != LOCAL_TENANT_ID
        || entity
            .get("tenant_id")
            .is_some_and(|tenant| tenant.as_str() != Some(LOCAL_TENANT_ID))
    {
        bail!("persistent state contains an unsupported tenant");
    }
    Ok(())
}

fn sqlite_version_tuple(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

fn changed_entries<K, T>(
    old: &BTreeMap<K, T>,
    new: &BTreeMap<K, T>,
    key: &str,
) -> anyhow::Result<Vec<(String, Value)>>
where
    K: Ord + Serialize + ToString,
    T: Eq + Serialize,
{
    if old.keys().any(|id| !new.contains_key(id)) {
        bail!("deleting {key} entities is forbidden");
    }
    new.iter()
        .filter(|(id, entity)| old.get(*id) != Some(*entity))
        .map(|(id, entity)| {
            let value = serde_json::to_value(entity)
                .with_context(|| format!("encode changed {key} entity"))?;
            Ok((id.to_string(), value))
        })
        .collect()
}

fn encode(value: &Value) -> anyhow::Result<String> {
    serde_json::to_string(value).context("encode domain entity")
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn optional_string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    string_field(value, key)
}

fn optional_id_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        })
}

fn unsigned_field(value: &Value, key: &str) -> i64 {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|number| i64::try_from(number).ok())
        .unwrap_or_default()
}

fn bool_field_value(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or_default()
}

fn enum_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| value.get(key).map(Value::to_string).unwrap_or_default())
}

fn enum_or_json_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .map(|value| match value {
            Value::String(value) => value.clone(),
            _ => value.to_string(),
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use chrono::Utc;
    use uuid::Uuid;

    use super::*;
    use crate::{
        domain::{
            CoverageMode, Lane, LineageRelation, ParameterSpec, Priority, RequestContext, State,
            SubmitInput, Target, TargetAuthorization,
        },
        engine::Engine,
        lineage::CoverageContext,
    };

    fn lane() -> Lane {
        Lane {
            tenant_id: crate::domain::default_local_tenant(),
            id: "test".into(),
            project: "project".into(),
            environment: "test".into(),
            enabled: true,
            repository_id: "repo".into(),
            release_ref: "refs/heads/release".into(),
            release_line_id: "repo:release".into(),
            profile_digest: "profile-v1".into(),
            config_version: 1,
            descendant_coverage_enabled: true,
            preemption_enabled: true,
            allowed_revisions: BTreeSet::new(),
            allowed_components: BTreeSet::from(["app".into()]),
            request_parameters: BTreeMap::new(),
            active_run_id: None,
            generation: 0,
            blocked_reason: None,
            failure_paused: false,
        }
    }

    fn submit(revision: &str, hash: &str) -> SubmitInput {
        SubmitInput {
            context: Default::default(),
            id: Uuid::new_v4(),
            principal: "operator".into(),
            idempotency_key: "key-1".into(),
            request_hash: hash.into(),
            lane_id: "test".into(),
            target: Target {
                repository_id: "repo".into(),
                revision: revision.into(),
                components: BTreeSet::from(["app".into()]),
                profile_digest: "profile-v1".into(),
                release_line_id: "repo:release".into(),
                config_version: 1,
                coverage_mode: CoverageMode::Exact,
                parameters: BTreeMap::new(),
            },
            authorization: TargetAuthorization {
                repository_id: "repo".into(),
                release_ref: "refs/heads/release".into(),
                release_tip: revision.into(),
                revision: revision.into(),
                release_line_id: "repo:release".into(),
                config_version: 1,
                relation: LineageRelation::Ancestor,
                proof_digest: "trusted-test-proof".into(),
            },
            priority: Priority::Normal,
            deadline_at: None,
            reason: "test".into(),
        }
    }

    #[tokio::test]
    async fn configured_request_parameters_round_trip_in_normalized_ledger() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("parameters.db");
        let store = Store::open(&database).await.unwrap();
        let mut state = State::default();
        let previous = state.clone();
        let engine = Engine::default();
        let mut configured_lane = lane();
        configured_lane.request_parameters.insert(
            "database_backup".into(),
            ParameterSpec {
                required: true,
                allowed_values: vec!["include".into(), "skip".into()],
            },
        );
        engine
            .register_lane(&mut state, configured_lane, Utc::now())
            .unwrap();
        let mut input = submit(&"a".repeat(40), "backup-policy");
        input.target.parameters = BTreeMap::from([("database_backup".into(), "skip".into())]);
        engine
            .submit(&mut state, input, Utc::now(), &CoverageContext::default())
            .unwrap();
        store.commit_transition(&previous, &state).await.unwrap();
        drop(store);

        let reopened = Store::open(&database).await.unwrap();
        let restored = reopened.load_state().await.unwrap();
        assert_eq!(restored, state);
        assert_eq!(
            restored.lanes["test"].request_parameters["database_backup"].allowed_values,
            ["include", "skip"]
        );
        assert_eq!(
            restored.requests.values().next().unwrap().target.parameters["database_backup"],
            "skip"
        );
    }

    #[tokio::test]
    async fn normalized_ledger_restarts_with_next_event_sequence_and_idempotency() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("state.db");
        let store = Store::open(&database).await.unwrap();
        let pragmas = store.pragma_status().await.unwrap();
        assert!(pragmas.foreign_keys);
        assert!(pragmas.synchronous_full);
        assert!(pragmas.wal);
        assert!(sqlite_version_tuple(&pragmas.version).unwrap() >= (3, 51, 3));

        let previous = State::default();
        let mut state = previous.clone();
        let engine = Engine::default();
        engine
            .register_lane(&mut state, lane(), Utc::now())
            .unwrap();
        let revision = "a".repeat(40);
        let first = engine
            .submit(
                &mut state,
                submit(&revision, "hash-1"),
                Utc::now(),
                &CoverageContext::default(),
            )
            .unwrap();
        let reused = engine
            .submit(
                &mut state,
                submit(&revision, "hash-1"),
                Utc::now(),
                &CoverageContext::default(),
            )
            .unwrap();
        assert_eq!(first.request_id, reused.request_id);
        assert!(reused.reused);
        assert!(
            engine
                .submit(
                    &mut state,
                    submit(&revision, "different-hash"),
                    Utc::now(),
                    &CoverageContext::default()
                )
                .is_err()
        );

        store.commit_transition(&previous, &state).await.unwrap();
        drop(store);
        let reopened = Store::open(&database).await.unwrap();
        let restored = reopened.load_state().await.unwrap();
        assert_eq!(restored, state);
        assert_eq!(restored.next_event_sequence.get("test"), Some(&3));

        let before_drain = restored.clone();
        let mut drained = restored;
        engine.set_draining(&mut drained, true, Utc::now());
        reopened
            .commit_transition(&before_drain, &drained)
            .await
            .unwrap();
        let restored = reopened.load_state().await.unwrap();
        assert!(restored.draining);
        assert_eq!(restored.events.last().map(|event| event.sequence), Some(3));
        assert!(reopened.pragma_status().await.unwrap().wal);
    }

    #[tokio::test]
    async fn request_context_and_actor_survive_restart_and_replay_keeps_creation_session() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("context.db");
        let store = Store::open(&database).await.unwrap();
        let empty = State::default();
        let mut state = empty.clone();
        let engine = Engine::default();
        engine
            .register_lane(&mut state, lane(), Utc::now())
            .unwrap();
        let revision = "a".repeat(40);
        let mut input = submit(&revision, "context-hash");
        input.context = RequestContext {
            task_id: Some("release-task-42".into()),
            session_id: Some("original-session".into()),
            collaborators: BTreeSet::from(["agent-b".into(), "agent-c".into()]),
        };
        let outcome = engine
            .submit(
                &mut state,
                input.clone(),
                Utc::now(),
                &CoverageContext::default(),
            )
            .unwrap();
        state.events.last_mut().unwrap().actor = Some("operator".into());
        store.commit_transition(&empty, &state).await.unwrap();
        let task: String = sqlx::query_scalar("SELECT task_id FROM requests WHERE id = ?")
            .bind(outcome.request_id.to_string())
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(task, "release-task-42");
        store.pool.close().await;

        let reopened = Store::open(&database).await.unwrap();
        let mut restored = reopened.load_state().await.unwrap();
        assert_eq!(restored, state);
        input.id = Uuid::new_v4();
        input.context.session_id = Some("replacement-session".into());
        let replay = engine
            .submit(
                &mut restored,
                input,
                Utc::now(),
                &CoverageContext::default(),
            )
            .unwrap();
        assert!(replay.reused);
        assert_eq!(replay.request_id, outcome.request_id);
        assert_eq!(restored, state);
    }

    #[tokio::test]
    async fn unsupported_tenant_is_rejected_before_commit_and_during_load() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("tenant.db")).await.unwrap();
        let empty = State::default();
        let mut baseline = empty.clone();
        let engine = Engine::default();
        let mut foreign_lane = lane();
        foreign_lane.tenant_id = "other-team".into();
        assert!(
            engine
                .register_lane(&mut baseline, foreign_lane, Utc::now())
                .is_err()
        );
        assert_eq!(baseline, empty);
        engine
            .register_lane(&mut baseline, lane(), Utc::now())
            .unwrap();
        let outcome = engine
            .submit(
                &mut baseline,
                submit(&"a".repeat(40), "hash"),
                Utc::now(),
                &CoverageContext::default(),
            )
            .unwrap();
        store.commit_transition(&empty, &baseline).await.unwrap();
        let mut invalid = baseline.clone();
        invalid
            .requests
            .get_mut(&outcome.request_id)
            .unwrap()
            .tenant_id = "other-team".into();
        assert!(store.commit_transition(&baseline, &invalid).await.is_err());
        assert_eq!(store.load_state().await.unwrap(), baseline);
        assert!(
            sqlx::query("UPDATE lanes SET tenant_id = 'other-team' WHERE id = 'test'")
                .execute(&store.pool)
                .await
                .is_err()
        );
        sqlx::query("UPDATE requests SET entity_json = json_set(entity_json, '$.tenant_id', 'other-team') WHERE id = ?")
            .bind(outcome.request_id.to_string()).execute(&store.pool).await.unwrap();
        assert!(store.load_state().await.is_err());
        let mut foreign_json = serde_json::to_value(lane()).unwrap();
        foreign_json["tenant_id"] = json!("other-team");
        assert!(serde_json::from_value::<Lane>(foreign_json).is_err());
    }

    #[tokio::test]
    async fn v1_ledger_migrates_without_rewriting_old_json_hashes_or_events() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("v1.db");
        let old_migrations = temp.path().join("old-migrations");
        std::fs::create_dir(&old_migrations).unwrap();
        std::fs::write(
            old_migrations.join("0001_initial.sql"),
            include_str!("../migrations/0001_initial.sql"),
        )
        .unwrap();
        let old_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&database)
                    .create_if_missing(true)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        Migrator::new(old_migrations.as_path())
            .await
            .unwrap()
            .run(&old_pool)
            .await
            .unwrap();
        let checksum: Vec<u8> =
            sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 1")
                .fetch_one(&old_pool)
                .await
                .unwrap();

        let engine = Engine::default();
        let mut expected = State::default();
        let lane = lane();
        engine
            .register_lane(&mut expected, lane.clone(), Utc::now())
            .unwrap();
        let revision = "a".repeat(40);
        let outcome = engine
            .submit(
                &mut expected,
                submit(&revision, "unchanged-v1-hash"),
                Utc::now(),
                &CoverageContext::default(),
            )
            .unwrap();
        let mut lane_json = serde_json::to_value(&lane).unwrap();
        lane_json.as_object_mut().unwrap().remove("tenant_id");
        sqlx::query("INSERT INTO lanes (id, project, environment, repository_id, release_line_id, release_ref, profile_digest, entity_json, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&lane.id).bind(&lane.project).bind(&lane.environment).bind(&lane.repository_id)
            .bind(&lane.release_line_id).bind(&lane.release_ref).bind(&lane.profile_digest)
            .bind(lane_json.to_string()).bind(Utc::now().to_rfc3339()).execute(&old_pool).await.unwrap();
        let request = &expected.requests[&outcome.request_id];
        let mut request_json = serde_json::to_value(request).unwrap();
        request_json.as_object_mut().unwrap().remove("tenant_id");
        request_json.as_object_mut().unwrap().remove("context");
        sqlx::query("INSERT INTO requests (id, lane_id, principal, idempotency_key, request_hash, status, created_at, entity_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(request.id.to_string()).bind(&request.lane_id).bind(&request.principal)
            .bind(&request.idempotency_key).bind(&request.request_hash).bind("waiting")
            .bind(request.created_at.to_rfc3339()).bind(request_json.to_string()).execute(&old_pool).await.unwrap();
        let mut event_jsons = Vec::new();
        for event in &expected.events {
            let mut event_json = serde_json::to_value(event).unwrap();
            event_json.as_object_mut().unwrap().remove("actor");
            let event_json = event_json.to_string();
            sqlx::query("INSERT INTO events (id, lane_id, sequence, run_id, request_id, timestamp, kind, message, entity_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
                .bind(format!("{}:{}", event.lane_id, event.sequence)).bind(&event.lane_id)
                .bind(i64::try_from(event.sequence).unwrap()).bind(event.run_id.map(|id| id.to_string()))
                .bind(event.request_id.map(|id| id.to_string())).bind(event.timestamp.to_rfc3339())
                .bind(&event.kind).bind(&event.message).bind(&event_json).execute(&old_pool).await.unwrap();
            event_jsons.push(event_json);
        }
        old_pool.close().await;

        let migrated = Store::open(&database).await.unwrap();
        let mut restored = migrated.load_state().await.unwrap();
        assert_eq!(restored, expected);
        assert_eq!(
            restored.requests[&outcome.request_id].context,
            RequestContext::default()
        );
        let restored_checksum: Vec<u8> =
            sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 1")
                .fetch_one(&migrated.pool)
                .await
                .unwrap();
        assert_eq!(restored_checksum, checksum);
        let stored_events: Vec<String> =
            sqlx::query_scalar("SELECT entity_json FROM events ORDER BY sequence")
                .fetch_all(&migrated.pool)
                .await
                .unwrap();
        assert_eq!(stored_events, event_jsons);
        let replay = engine
            .submit(
                &mut restored,
                submit(&revision, "unchanged-v1-hash"),
                Utc::now(),
                &CoverageContext::default(),
            )
            .unwrap();
        assert!(replay.reused);
        assert_eq!(replay.request_id, outcome.request_id);
        assert_eq!(restored, expected);
        assert!(
            engine
                .submit(
                    &mut restored,
                    submit(&revision, "changed-hash"),
                    Utc::now(),
                    &CoverageContext::default()
                )
                .is_err()
        );
    }

    #[tokio::test]
    async fn resource_registry_rejects_overlaps_and_keeps_append_only_audit() {
        use crate::resources::{ManagedResource, ResourceKind, ResourcePin, ResourcePinKind};

        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("resources.db"))
            .await
            .unwrap();
        let now = Utc::now();
        let resource = |id: &str, path: &str| ManagedResource {
            schema_version: 1,
            id: id.into(),
            root_id: "cache".into(),
            relative_path: path.into(),
            canonical_path: format!("/managed/{path}"),
            owner: "autotask".into(),
            project: "project".into(),
            kind: ResourceKind::BuildCache,
            input_digest: "sha256:test".into(),
            toolchain: "rustc-1.98".into(),
            platform: "linux-x86_64".into(),
            observed_bytes: 64,
            last_used_at: now,
            recreatable: true,
            execution_ref: None,
            created_at: now,
            updated_at: now,
            pins: Vec::new(),
        };

        store
            .register_resource(resource("cache-one", "cache"), "admin")
            .await
            .unwrap();
        let overlap = store
            .register_resource(resource("cache-child", "cache/child"), "admin")
            .await
            .unwrap_err();
        assert!(matches!(overlap, ResourceStoreError::Conflict(_)));

        store
            .set_resource_pin(
                "cache-one",
                &ResourcePin {
                    kind: ResourcePinKind::CurrentRelease,
                    reference_id: "release-sha".into(),
                },
                true,
                "admin",
                now,
            )
            .await
            .unwrap();
        let events = store
            .list_resource_events("cache-one", 0, 10)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.action.as_str())
                .collect::<Vec<_>>(),
            ["registered", "pin_added"]
        );
        assert!(
            sqlx::query("UPDATE resource_events SET detail = 'rewritten' WHERE resource_id = ?")
                .bind("cache-one")
                .execute(&store.pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("DELETE FROM resource_events WHERE resource_id = ?")
                .bind("cache-one")
                .execute(&store.pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("DELETE FROM managed_resources WHERE id = ?")
                .bind("cache-one")
                .execute(&store.pool)
                .await
                .is_err()
        );
        assert_eq!(
            store
                .list_resource_events("cache-one", 0, 10)
                .await
                .unwrap()
                .len(),
            2
        );
    }
}
