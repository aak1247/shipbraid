PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS lanes (
    id TEXT PRIMARY KEY NOT NULL,
    project TEXT NOT NULL,
    environment TEXT NOT NULL,
    repository_id TEXT NOT NULL,
    release_line_id TEXT NOT NULL,
    release_ref TEXT NOT NULL,
    config_version INTEGER NOT NULL DEFAULT 0 CHECK (config_version >= 0),
    profile_digest TEXT NOT NULL,
    active_run_id TEXT,
    generation INTEGER NOT NULL DEFAULT 0 CHECK (generation >= 0),
    blocked_reason TEXT,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    failure_paused INTEGER NOT NULL DEFAULT 0 CHECK (failure_paused IN (0, 1)),
    entity_json TEXT NOT NULL CHECK (json_valid(entity_json)),
    updated_at TEXT NOT NULL,
    FOREIGN KEY (active_run_id) REFERENCES runs(id)
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE IF NOT EXISTS requests (
    id TEXT PRIMARY KEY NOT NULL,
    lane_id TEXT NOT NULL,
    principal TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    status TEXT NOT NULL,
    current_run_id TEXT,
    satisfied_by_run TEXT,
    waiting_reason TEXT,
    created_at TEXT NOT NULL,
    entity_json TEXT NOT NULL CHECK (json_valid(entity_json)),
    UNIQUE (principal, lane_id, idempotency_key),
    FOREIGN KEY (lane_id) REFERENCES lanes(id) DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (current_run_id) REFERENCES runs(id) DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX IF NOT EXISTS requests_lane_status_created
    ON requests(lane_id, status, created_at DESC);
CREATE INDEX IF NOT EXISTS requests_run ON requests(current_run_id);

CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY NOT NULL,
    lane_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation >= 0),
    state TEXT NOT NULL,
    phase TEXT NOT NULL,
    replacement_run_id TEXT,
    created_at TEXT NOT NULL,
    phase_started_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    entity_json TEXT NOT NULL CHECK (json_valid(entity_json)),
    UNIQUE (lane_id, generation),
    FOREIGN KEY (lane_id) REFERENCES lanes(id) DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (replacement_run_id) REFERENCES runs(id) DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX IF NOT EXISTS runs_lane_created ON runs(lane_id, created_at DESC);
CREATE INDEX IF NOT EXISTS runs_state ON runs(state);

CREATE TABLE IF NOT EXISTS receipts (
    id TEXT PRIMARY KEY NOT NULL,
    run_id TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    result TEXT NOT NULL,
    entity_json TEXT NOT NULL CHECK (json_valid(entity_json)),
    FOREIGN KEY (run_id) REFERENCES runs(id) DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX IF NOT EXISTS receipts_run_observed ON receipts(run_id, observed_at DESC);

CREATE TABLE IF NOT EXISTS commands (
    id TEXT PRIMARY KEY NOT NULL,
    run_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation >= 0),
    kind TEXT NOT NULL,
    delivery_state TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    created_at TEXT NOT NULL,
    entity_json TEXT NOT NULL CHECK (json_valid(entity_json)),
    FOREIGN KEY (run_id) REFERENCES runs(id) DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX IF NOT EXISTS commands_pending ON commands(delivery_state, created_at);

CREATE TABLE IF NOT EXISTS events (
    id TEXT PRIMARY KEY NOT NULL,
    lane_id TEXT NOT NULL,
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    run_id TEXT,
    request_id TEXT,
    timestamp TEXT NOT NULL,
    kind TEXT NOT NULL,
    message TEXT NOT NULL,
    entity_json TEXT NOT NULL CHECK (json_valid(entity_json)),
    UNIQUE (lane_id, sequence),
    FOREIGN KEY (lane_id) REFERENCES lanes(id) DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (run_id) REFERENCES runs(id) DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (request_id) REFERENCES requests(id) DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX IF NOT EXISTS events_lane_sequence ON events(lane_id, sequence);

-- Service-wide operational flags are metadata, not a seventh entity ledger.
CREATE TABLE IF NOT EXISTS runtime_meta (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);

CREATE TRIGGER IF NOT EXISTS events_no_update
BEFORE UPDATE ON events BEGIN
    SELECT RAISE(ABORT, 'events are append-only');
END;
CREATE TRIGGER IF NOT EXISTS events_no_delete
BEFORE DELETE ON events BEGIN
    SELECT RAISE(ABORT, 'events are append-only');
END;
