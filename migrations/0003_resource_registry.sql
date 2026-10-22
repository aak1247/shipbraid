PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS managed_resources (
    id TEXT PRIMARY KEY NOT NULL,
    root_id TEXT NOT NULL,
    canonical_path TEXT NOT NULL,
    owner TEXT NOT NULL,
    project TEXT NOT NULL,
    kind TEXT NOT NULL,
    observed_bytes INTEGER NOT NULL CHECK (observed_bytes >= 0),
    last_used_at TEXT NOT NULL,
    recreatable INTEGER NOT NULL CHECK (recreatable IN (0, 1)),
    entity_json TEXT NOT NULL CHECK (json_valid(entity_json)),
    UNIQUE (root_id, canonical_path)
);
CREATE INDEX IF NOT EXISTS managed_resources_root_lru
    ON managed_resources(root_id, last_used_at, id);

CREATE TRIGGER IF NOT EXISTS managed_resources_no_delete
BEFORE DELETE ON managed_resources BEGIN
    SELECT RAISE(ABORT, 'managed resource registrations are retained');
END;

CREATE TABLE IF NOT EXISTS resource_pins (
    resource_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    reference_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (resource_id, kind, reference_id),
    FOREIGN KEY (resource_id) REFERENCES managed_resources(id)
        DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX IF NOT EXISTS resource_pins_resource ON resource_pins(resource_id);

CREATE TABLE IF NOT EXISTS resource_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    resource_id TEXT NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    timestamp TEXT NOT NULL,
    detail TEXT NOT NULL,
    FOREIGN KEY (resource_id) REFERENCES managed_resources(id)
        DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX IF NOT EXISTS resource_events_resource_sequence
    ON resource_events(resource_id, sequence);

CREATE TRIGGER IF NOT EXISTS resource_events_no_update
BEFORE UPDATE ON resource_events BEGIN
    SELECT RAISE(ABORT, 'resource events are append-only');
END;
CREATE TRIGGER IF NOT EXISTS resource_events_no_delete
BEFORE DELETE ON resource_events BEGIN
    SELECT RAISE(ABORT, 'resource events are append-only');
END;
