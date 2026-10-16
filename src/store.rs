use std::{path::Path, time::Duration};

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};
use sqlx::{
    Row,
    migrate::Migrator,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::domain::State;

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
            self.read_map("SELECT id, entity_json FROM lanes ORDER BY id")
                .await?,
        );
        object.insert(
            "requests".into(),
            self.read_map("SELECT id, entity_json FROM requests ORDER BY id")
                .await?,
        );
        object.insert(
            "runs".into(),
            self.read_map("SELECT id, entity_json FROM runs ORDER BY id")
                .await?,
        );
        object.insert(
            "receipts".into(),
            self.read_map("SELECT id, entity_json FROM receipts ORDER BY id")
                .await?,
        );
        object.insert(
            "commands".into(),
            self.read_map("SELECT id, entity_json FROM commands ORDER BY id")
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
        let previous_root = serde_json::to_value(previous)?;
        let old_object = previous_root
            .as_object()
            .context("domain State must serialize to an object")?;
        let root = serde_json::to_value(next)?;
        let object = root
            .as_object()
            .context("domain State must serialize to an object")?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .context("begin immediate state transaction")?;

        for (id, value) in changed_entries(old_object, object, "lanes")? {
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

        for (id, value) in changed_entries(old_object, object, "runs")? {
            let entity = encode(value)?;
            let run_id = string_field(value, "id").unwrap_or(id);
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

        for (id, value) in changed_entries(old_object, object, "requests")? {
            let entity = encode(value)?;
            let request_id = string_field(value, "id").unwrap_or(id);
            sqlx::query(
                "INSERT INTO requests (id, lane_id, principal, idempotency_key, request_hash, status, current_run_id, satisfied_by_run, waiting_reason, created_at, entity_json) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(id) DO UPDATE SET lane_id=excluded.lane_id, principal=excluded.principal, idempotency_key=excluded.idempotency_key, request_hash=excluded.request_hash, status=excluded.status, current_run_id=excluded.current_run_id, satisfied_by_run=excluded.satisfied_by_run, waiting_reason=excluded.waiting_reason, entity_json=excluded.entity_json \
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
            .execute(&mut *tx)
            .await?;
        }

        for (id, value) in changed_entries(old_object, object, "receipts")? {
            let entity = encode(value)?;
            let receipt_id = string_field(value, "id").unwrap_or(id);
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

        for (id, value) in changed_entries(old_object, object, "commands")? {
            let entity = encode(value)?;
            let command_id = string_field(value, "id").unwrap_or(id);
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

        let old_events = array_field(old_object, "events")?;
        let new_events = array_field(object, "events")?;
        if new_events.len() < old_events.len() || &new_events[..old_events.len()] != old_events {
            bail!("event history must remain append-only");
        }
        for value in &new_events[old_events.len()..] {
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

        if bool_field(old_object, "draining") != bool_field(object, "draining") {
            sqlx::query(
                "INSERT INTO runtime_meta(key, value) VALUES ('draining', ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            )
            .bind(if bool_field(object, "draining") { "true" } else { "false" })
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await.context("commit state transaction")
    }

    async fn read_map(&self, query: &'static str) -> anyhow::Result<Value> {
        let rows = sqlx::query(query).fetch_all(&self.pool).await?;
        let mut entities = Map::new();
        for row in rows {
            let id: String = row.try_get("id")?;
            let value: String = row.try_get("entity_json")?;
            entities.insert(id, serde_json::from_str(&value)?);
        }
        Ok(Value::Object(entities))
    }

    async fn read_events(&self) -> anyhow::Result<Value> {
        let rows = sqlx::query("SELECT entity_json FROM events ORDER BY lane_id, sequence")
            .fetch_all(&self.pool)
            .await?;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let value: String = row.try_get("entity_json")?;
            events.push(serde_json::from_str(&value)?);
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

fn sqlite_version_tuple(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

fn entity_map<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> anyhow::Result<&'a Map<String, Value>> {
    object
        .get(key)
        .and_then(Value::as_object)
        .with_context(|| format!("domain State is missing object field `{key}`"))
}

fn changed_entries<'a>(
    old: &'a Map<String, Value>,
    new: &'a Map<String, Value>,
    key: &str,
) -> anyhow::Result<Vec<(&'a str, &'a Value)>> {
    let old_entities = entity_map(old, key)?;
    let new_entities = entity_map(new, key)?;
    if old_entities.keys().any(|id| !new_entities.contains_key(id)) {
        bail!("deleting {key} entities is forbidden");
    }
    Ok(new_entities
        .iter()
        .filter(|(id, value)| old_entities.get(*id) != Some(*value))
        .map(|(id, value)| (id.as_str(), value))
        .collect())
}

fn array_field<'a>(object: &'a Map<String, Value>, key: &str) -> anyhow::Result<&'a [Value]> {
    object
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .with_context(|| format!("domain State is missing array field `{key}`"))
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

fn bool_field(value: &Map<String, Value>, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or_default()
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
    use std::collections::BTreeSet;

    use chrono::Utc;
    use uuid::Uuid;

    use super::*;
    use crate::{
        domain::{
            CoverageMode, Lane, LineageRelation, Priority, State, SubmitInput, Target,
            TargetAuthorization,
        },
        engine::Engine,
        lineage::CoverageContext,
    };

    fn lane() -> Lane {
        Lane {
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
            active_run_id: None,
            generation: 0,
            blocked_reason: None,
            failure_paused: false,
        }
    }

    fn submit(revision: &str, hash: &str) -> SubmitInput {
        SubmitInput {
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
}
