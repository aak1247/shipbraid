-- Reserve the tenant boundary while this release supports one local team only.
-- Existing entity JSON and append-only event history remain untouched.
ALTER TABLE lanes ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'local' CHECK (tenant_id = 'local');
ALTER TABLE requests ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'local' CHECK (tenant_id = 'local');
ALTER TABLE runs ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'local' CHECK (tenant_id = 'local');
ALTER TABLE receipts ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'local' CHECK (tenant_id = 'local');
ALTER TABLE commands ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'local' CHECK (tenant_id = 'local');
ALTER TABLE events ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'local' CHECK (tenant_id = 'local');

ALTER TABLE requests ADD COLUMN task_id TEXT;
CREATE INDEX requests_tenant_task_created ON requests(tenant_id, task_id, created_at DESC);
