-- 0052 — B3 T5: the tenant's knowledge-store tree (spec 16 §2 `knowledge_store`, T; live data, D69) and its direct
-- uploads (T6 writes them; T5 reads them for a document's language, G5, and ingest job).
--
-- `knowledge_store` is agent's own tree: `group` stores hold stores, `leaf` stores hold documents. Each row is bound to
-- one strata_rag store (`strata_store_id`, set by the `store_ensure` outbox sender, crate::provision::senders::stores)
-- or unbound (NULL: not created in strata yet). agent `group` = strata `router`, agent `key` = strata `name`, `label`
-- = strata `description`. `ensuring` is derived (an open `store_ensure` outbox row), never stored. `region` is the
-- tenant's residency at creation (D59/D66). A tenant may hold several root stores (plan §0 R7).
--
-- Drift-tolerant as 0050 (IF NOT EXISTS, DROP … IF EXISTS + ADD, DROP POLICY IF EXISTS + CREATE, REVOKE ALL +
-- GRANT). References nothing from 0091–0093 (tests/migration_order.rs). Forward-only.

CREATE TABLE IF NOT EXISTS knowledge_store (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    id              uuid NOT NULL,
    key             text NOT NULL,
    label           text NOT NULL,
    parent_id       uuid,
    kind            text NOT NULL,
    areas           text[] NOT NULL DEFAULT '{}',
    strata_store_id uuid,
    region          text NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_by      text NOT NULL,
    PRIMARY KEY (id)
);
-- Constraints that depend on `knowledge_store_tenant_id_key` go first, so a re-run can drop and re-add it.
ALTER TABLE IF EXISTS knowledge_upload DROP CONSTRAINT IF EXISTS knowledge_upload_store;
ALTER TABLE knowledge_store DROP CONSTRAINT IF EXISTS knowledge_store_parent;
ALTER TABLE knowledge_store
    DROP CONSTRAINT IF EXISTS knowledge_store_tenant,
    ADD CONSTRAINT knowledge_store_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS knowledge_store_tenant_id_key,
    ADD CONSTRAINT knowledge_store_tenant_id_key UNIQUE (tenant_id, id),
    DROP CONSTRAINT IF EXISTS knowledge_store_tenant_key_key,
    ADD CONSTRAINT knowledge_store_tenant_key_key UNIQUE (tenant_id, key),
    DROP CONSTRAINT IF EXISTS knowledge_store_tenant_strata_key,
    ADD CONSTRAINT knowledge_store_tenant_strata_key UNIQUE (tenant_id, strata_store_id),
    ADD CONSTRAINT knowledge_store_parent FOREIGN KEY (tenant_id, parent_id)
        REFERENCES knowledge_store (tenant_id, id),
    DROP CONSTRAINT IF EXISTS knowledge_store_key_check,
    ADD CONSTRAINT knowledge_store_key_check CHECK (key ~ '^[a-z][a-z0-9_.-]{1,62}$'),
    DROP CONSTRAINT IF EXISTS knowledge_store_label_check,
    ADD CONSTRAINT knowledge_store_label_check CHECK (char_length(btrim(label)) BETWEEN 1 AND 200),
    DROP CONSTRAINT IF EXISTS knowledge_store_kind_check,
    ADD CONSTRAINT knowledge_store_kind_check CHECK (kind IN ('group', 'leaf')),
    DROP CONSTRAINT IF EXISTS knowledge_store_parent_self_check,
    ADD CONSTRAINT knowledge_store_parent_self_check CHECK (parent_id IS DISTINCT FROM id),
    DROP CONSTRAINT IF EXISTS knowledge_store_areas_check,
    ADD CONSTRAINT knowledge_store_areas_check
        CHECK (cardinality(areas) <= 64 AND array_position(areas, NULL) IS NULL),
    DROP CONSTRAINT IF EXISTS knowledge_store_region_check,
    ADD CONSTRAINT knowledge_store_region_check CHECK (region IN ('eu', 'us')),
    DROP CONSTRAINT IF EXISTS knowledge_store_updated_by_check,
    ADD CONSTRAINT knowledge_store_updated_by_check CHECK (btrim(updated_by) <> '');
CREATE INDEX IF NOT EXISTS knowledge_store_parent_idx ON knowledge_store (tenant_id, parent_id);
ALTER TABLE knowledge_store ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON knowledge_store;
CREATE POLICY tenant_isolation ON knowledge_store FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON knowledge_store FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON knowledge_store TO agent_app;

-- One direct upload into a leaf (T6, `uploadToStore`): strata_ingest is called with `source_id = upload:<id>`, so a
-- document's language (D68, G5) and its ingest job resolve from here. `files` = [{name, bytes, content_type,
-- ingest_job_id, error}]. Deleting the store deletes its upload records.
CREATE TABLE IF NOT EXISTS knowledge_upload (
    tenant_id      uuid NOT NULL DEFAULT app_current_tenant(),
    id             uuid NOT NULL,
    store_id       uuid NOT NULL,
    language       text NOT NULL,
    files          jsonb NOT NULL DEFAULT '[]',
    ingest_job_ids text[] NOT NULL DEFAULT '{}',
    created_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
    created_by     text NOT NULL,
    PRIMARY KEY (id)
);
ALTER TABLE knowledge_upload
    DROP CONSTRAINT IF EXISTS knowledge_upload_tenant,
    ADD CONSTRAINT knowledge_upload_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS knowledge_upload_store,
    ADD CONSTRAINT knowledge_upload_store FOREIGN KEY (tenant_id, store_id)
        REFERENCES knowledge_store (tenant_id, id) ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS knowledge_upload_language_check,
    ADD CONSTRAINT knowledge_upload_language_check CHECK (language ~ '^[a-z]{2,3}(-[A-Za-z0-9]{2,8})*$'),
    DROP CONSTRAINT IF EXISTS knowledge_upload_files_check,
    ADD CONSTRAINT knowledge_upload_files_check CHECK (jsonb_typeof(files) = 'array'),
    DROP CONSTRAINT IF EXISTS knowledge_upload_created_by_check,
    ADD CONSTRAINT knowledge_upload_created_by_check CHECK (btrim(created_by) <> '');
CREATE INDEX IF NOT EXISTS knowledge_upload_store_idx ON knowledge_upload (tenant_id, store_id);
ALTER TABLE knowledge_upload ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON knowledge_upload;
CREATE POLICY tenant_isolation ON knowledge_upload FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON knowledge_upload FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON knowledge_upload TO agent_app;
