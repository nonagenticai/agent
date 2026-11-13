-- 0053 — B3 T7/T8: a job's knowledge sources and their re-sync runs (plan B3 §4; ports care-bus-agents@96c608c
-- `migrations/0003_sources.sql` with tenant + job scope, the D68 `language`, and the §0 R2 sealed credentials).
--
-- `source` is live data (D69): a customer feed (website, api, datatable) or a file source whose documents land in one
-- LEAF `knowledge_store` of the tenant. Credentials are write-only: one keyring value (`v2:<kid>:…`, crate::providers::
-- secret, AAD `<tenant>:source_cred:<source id>`) plus its fingerprint and when it was set; never selected into a
-- response. `schedule` is a cron that is validated, stored and shown but never run (D82: the scheduler is off).
-- `source_run` is one re-sync (T8 writes it; T7 reads the last one and refuses a delete while one is running).
--
-- Both tables are TENANT (RLS `tenant_isolation`, as `jobs`); job scope is the explicit `job_id` filter of every
-- query (composite FKs keep a row inside its tenant and job). Drift-tolerant as 0050–0052 (IF NOT EXISTS, DROP …
-- IF EXISTS + ADD, DROP POLICY IF EXISTS + CREATE, REVOKE ALL + GRANT). References nothing from 0091–0093
-- (tests/migration_order.rs). Forward-only.

CREATE TABLE IF NOT EXISTS source (
    tenant_id              uuid NOT NULL DEFAULT app_current_tenant(),
    job_id                 uuid NOT NULL,
    id                     uuid NOT NULL,
    name                   text NOT NULL,
    type                   text NOT NULL,
    config                 jsonb NOT NULL DEFAULT '{}',
    mapping                jsonb NOT NULL DEFAULT '[]',
    target_store_id        uuid NOT NULL,
    schedule               text,
    enabled                boolean NOT NULL DEFAULT true,
    language               text NOT NULL,
    credentials_sealed     text,
    credentials_fpr        text,
    credentials_updated_at timestamptz,
    created_at             timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_at             timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_by             text NOT NULL,
    PRIMARY KEY (id)
);
-- `source_run_source` depends on `source_tenant_id_key`: drop it first so a re-run can re-add both.
ALTER TABLE IF EXISTS source_run DROP CONSTRAINT IF EXISTS source_run_source;
ALTER TABLE source
    DROP CONSTRAINT IF EXISTS source_tenant,
    ADD CONSTRAINT source_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS source_job,
    ADD CONSTRAINT source_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id) ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS source_store,
    ADD CONSTRAINT source_store FOREIGN KEY (tenant_id, target_store_id) REFERENCES knowledge_store (tenant_id, id),
    DROP CONSTRAINT IF EXISTS source_tenant_id_key,
    ADD CONSTRAINT source_tenant_id_key UNIQUE (tenant_id, id),
    DROP CONSTRAINT IF EXISTS source_job_name_key,
    ADD CONSTRAINT source_job_name_key UNIQUE (tenant_id, job_id, name),
    DROP CONSTRAINT IF EXISTS source_name_check,
    ADD CONSTRAINT source_name_check CHECK (char_length(btrim(name)) BETWEEN 1 AND 120 AND name = btrim(name)),
    DROP CONSTRAINT IF EXISTS source_type_check,
    ADD CONSTRAINT source_type_check CHECK (type IN ('website', 'api', 'file', 'datatable')),
    DROP CONSTRAINT IF EXISTS source_config_check,
    ADD CONSTRAINT source_config_check CHECK (jsonb_typeof(config) = 'object'),
    DROP CONSTRAINT IF EXISTS source_mapping_check,
    ADD CONSTRAINT source_mapping_check CHECK (jsonb_typeof(mapping) = 'array'),
    DROP CONSTRAINT IF EXISTS source_schedule_check,
    ADD CONSTRAINT source_schedule_check CHECK (schedule IS NULL OR char_length(schedule) BETWEEN 1 AND 100),
    DROP CONSTRAINT IF EXISTS source_language_check,
    ADD CONSTRAINT source_language_check CHECK (language ~ '^[a-z]{2,3}(-[A-Za-z0-9]{2,8})*$'),
    DROP CONSTRAINT IF EXISTS source_credentials_sealed_check,
    ADD CONSTRAINT source_credentials_sealed_check CHECK (credentials_sealed IS NULL OR credentials_sealed LIKE 'v2:%'),
    DROP CONSTRAINT IF EXISTS source_credentials_check,
    ADD CONSTRAINT source_credentials_check CHECK (
        (credentials_sealed IS NULL) = (credentials_fpr IS NULL)
        AND (credentials_sealed IS NULL) = (credentials_updated_at IS NULL)),
    DROP CONSTRAINT IF EXISTS source_updated_by_check,
    ADD CONSTRAINT source_updated_by_check CHECK (btrim(updated_by) <> '');
CREATE INDEX IF NOT EXISTS source_job_idx ON source (tenant_id, job_id, name);
CREATE INDEX IF NOT EXISTS source_store_idx ON source (tenant_id, target_store_id);
ALTER TABLE source ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON source;
CREATE POLICY tenant_isolation ON source FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON source FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON source TO agent_app;

-- One re-sync of a source (CB `source_runs` + tenant/job scope, `documents`, `pages`, `mapping`, `uploads`; `error` is
-- the UI's `{code, message}`). `document_ids` are the strata_rag documents the run's ingest jobs produced;
-- `documents_removed` turns true once a later run (or a purge) deleted them. At most one running run per source.
CREATE TABLE IF NOT EXISTS source_run (
    tenant_id         uuid NOT NULL DEFAULT app_current_tenant(),
    job_id            uuid NOT NULL,
    id                uuid NOT NULL,
    source_id         uuid NOT NULL,
    status            text NOT NULL,
    started_at        timestamptz NOT NULL DEFAULT clock_timestamp(),
    finished_at       timestamptz,
    started_by        text NOT NULL,
    ingest_job_ids    text[] NOT NULL DEFAULT '{}',
    document_ids      text[] NOT NULL DEFAULT '{}',
    documents         integer NOT NULL DEFAULT 0,
    pages             jsonb,
    error             jsonb,
    mapping           text,
    uploads           jsonb,
    replaced_run_id   uuid,
    replace_report    jsonb,
    documents_removed boolean NOT NULL DEFAULT false,
    PRIMARY KEY (id)
);
ALTER TABLE source_run
    DROP CONSTRAINT IF EXISTS source_run_tenant,
    ADD CONSTRAINT source_run_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS source_run_job,
    ADD CONSTRAINT source_run_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id) ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS source_run_source,
    ADD CONSTRAINT source_run_source FOREIGN KEY (tenant_id, source_id) REFERENCES source (tenant_id, id)
        ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS source_run_replaced,
    ADD CONSTRAINT source_run_replaced FOREIGN KEY (replaced_run_id) REFERENCES source_run (id) ON DELETE SET NULL,
    DROP CONSTRAINT IF EXISTS source_run_status_check,
    ADD CONSTRAINT source_run_status_check CHECK (status IN ('running', 'completed', 'failed', 'unsupported')),
    DROP CONSTRAINT IF EXISTS source_run_finished_check,
    ADD CONSTRAINT source_run_finished_check CHECK ((status = 'running') = (finished_at IS NULL)),
    DROP CONSTRAINT IF EXISTS source_run_documents_check,
    ADD CONSTRAINT source_run_documents_check CHECK (documents >= 0),
    DROP CONSTRAINT IF EXISTS source_run_pages_check,
    ADD CONSTRAINT source_run_pages_check CHECK (pages IS NULL OR jsonb_typeof(pages) = 'object'),
    DROP CONSTRAINT IF EXISTS source_run_error_check,
    ADD CONSTRAINT source_run_error_check CHECK (error IS NULL OR jsonb_typeof(error) = 'object'),
    DROP CONSTRAINT IF EXISTS source_run_mapping_check,
    ADD CONSTRAINT source_run_mapping_check CHECK (mapping IS NULL OR mapping IN ('same', 'edited')),
    DROP CONSTRAINT IF EXISTS source_run_uploads_check,
    ADD CONSTRAINT source_run_uploads_check CHECK (uploads IS NULL OR jsonb_typeof(uploads) = 'array'),
    DROP CONSTRAINT IF EXISTS source_run_replace_report_check,
    ADD CONSTRAINT source_run_replace_report_check
        CHECK (replace_report IS NULL OR jsonb_typeof(replace_report) = 'object'),
    DROP CONSTRAINT IF EXISTS source_run_started_by_check,
    ADD CONSTRAINT source_run_started_by_check CHECK (btrim(started_by) <> '');
CREATE UNIQUE INDEX IF NOT EXISTS source_run_one_running ON source_run (source_id) WHERE status = 'running';
CREATE INDEX IF NOT EXISTS source_run_by_source ON source_run (tenant_id, source_id, started_at DESC, id DESC);
ALTER TABLE source_run ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON source_run;
CREATE POLICY tenant_isolation ON source_run FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON source_run FROM agent_app;
GRANT SELECT, INSERT, UPDATE ON source_run TO agent_app;
