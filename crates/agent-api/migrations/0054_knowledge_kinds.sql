-- 0054 (B3 T10, plan §4 + §0 R10): knowledge-kind bindings (D68, D54 drafts) and the platform-pack cache.
--
-- `job_knowledge_kind` is the WORKING (draft) row of design kind `knowledge` (job scope, key `kind:<stage>:<kind>`,
-- value `{store, packs}`): one row per (job, stage, kind), materialised from the job template
-- (crate::knowledge::kinds::materialise, `origin = 'base'`) and bound by `bindKnowledgeKind` inside
-- `design::track`. The leaf set of a kind = `store_path` (the tenant leaf, precedence) + `packs` (platform pack pins
-- `pack_key@version`, explicit, never moved by anything but a bind). `packs_source = 'template'` rows take their
-- pins from the template on every re-materialise; `binding` rows are never overwritten by it.
--
-- `platform_pack` is an OBSERVED cache, not a draft: the tenant's last strata_rag `GET /api/platform/packs` answer
-- (`listed`), plus `unreadable_at` when a query found a pinned root unreadable (R11). Read by the leaf-set query
-- path and readiness (local state only).
--
-- Drift-tolerant as 0050 (IF NOT EXISTS, CREATE OR REPLACE FUNCTION, DROP … IF EXISTS + ADD, DROP POLICY IF EXISTS +
-- CREATE, REVOKE ALL + GRANT). References nothing from 0091–0093 (tests/migration_order.rs). Forward-only.

-- At most 16 pins, each `key@version` (key: up to six `/`-separated lowercase segments; version 1..999999999), no
-- key twice.
CREATE OR REPLACE FUNCTION knowledge_pack_pins_valid(p text[]) RETURNS boolean
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    SET search_path = pg_catalog, public, pg_temp
    AS $$
    SELECT p IS NOT NULL
       AND cardinality(p) <= 16
       AND NOT EXISTS (
           SELECT 1 FROM unnest(p) AS e
            WHERE e IS NULL OR e !~ '^[a-z0-9_-]+(/[a-z0-9_-]+){0,5}@[1-9][0-9]{0,8}$')
       AND (SELECT count(DISTINCT split_part(e, '@', 1)) FROM unnest(p) AS e) = cardinality(p)
$$;

CREATE TABLE IF NOT EXISTS job_knowledge_kind (
    tenant_id    uuid NOT NULL DEFAULT app_current_tenant(),
    job_id       uuid NOT NULL,
    stage        text NOT NULL,
    kind         text NOT NULL,
    required     boolean NOT NULL DEFAULT false,
    description  text NOT NULL DEFAULT '',
    origin       text NOT NULL DEFAULT 'tenant',
    store_path   text,
    packs        text[] NOT NULL DEFAULT '{}',
    packs_source text NOT NULL DEFAULT 'binding',
    updated_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_by   text NOT NULL,
    PRIMARY KEY (tenant_id, job_id, stage, kind)
);
ALTER TABLE job_knowledge_kind
    DROP CONSTRAINT IF EXISTS job_knowledge_kind_tenant,
    ADD CONSTRAINT job_knowledge_kind_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS job_knowledge_kind_job,
    ADD CONSTRAINT job_knowledge_kind_job FOREIGN KEY (tenant_id, job_id)
        REFERENCES jobs (tenant_id, id) ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS job_knowledge_kind_stage_check,
    ADD CONSTRAINT job_knowledge_kind_stage_check CHECK (char_length(stage) BETWEEN 1 AND 64 AND strpos(stage, ':') = 0 AND strpos(stage, '|') = 0 AND stage !~ '\s'),
    DROP CONSTRAINT IF EXISTS job_knowledge_kind_kind_check,
    ADD CONSTRAINT job_knowledge_kind_kind_check CHECK (char_length(kind) BETWEEN 1 AND 128 AND strpos(kind, ':') = 0 AND strpos(kind, '|') = 0 AND kind !~ '\s'),
    DROP CONSTRAINT IF EXISTS job_knowledge_kind_origin_check,
    ADD CONSTRAINT job_knowledge_kind_origin_check CHECK (origin IN ('base', 'tenant')),
    DROP CONSTRAINT IF EXISTS job_knowledge_kind_store_path_check,
    ADD CONSTRAINT job_knowledge_kind_store_path_check
        CHECK (store_path IS NULL OR char_length(store_path) BETWEEN 1 AND 1100),
    DROP CONSTRAINT IF EXISTS job_knowledge_kind_packs_check,
    ADD CONSTRAINT job_knowledge_kind_packs_check CHECK (knowledge_pack_pins_valid(packs)),
    DROP CONSTRAINT IF EXISTS job_knowledge_kind_packs_source_check,
    ADD CONSTRAINT job_knowledge_kind_packs_source_check CHECK (packs_source IN ('binding', 'template')),
    DROP CONSTRAINT IF EXISTS job_knowledge_kind_updated_by_check,
    ADD CONSTRAINT job_knowledge_kind_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE job_knowledge_kind ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON job_knowledge_kind;
CREATE POLICY tenant_isolation ON job_knowledge_kind FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON job_knowledge_kind FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON job_knowledge_kind TO agent_app;

CREATE TABLE IF NOT EXISTS platform_pack (
    tenant_id     uuid NOT NULL DEFAULT app_current_tenant(),
    pack_key      text NOT NULL,
    version       integer NOT NULL,
    root_id       uuid NOT NULL,
    published_at  timestamptz,
    listed        boolean NOT NULL,
    checked_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
    unreadable_at timestamptz,
    PRIMARY KEY (tenant_id, pack_key, version)
);
ALTER TABLE platform_pack
    DROP CONSTRAINT IF EXISTS platform_pack_tenant,
    ADD CONSTRAINT platform_pack_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS platform_pack_key_check,
    ADD CONSTRAINT platform_pack_key_check CHECK (pack_key ~ '^[a-z0-9_-]+(/[a-z0-9_-]+){0,5}$'),
    DROP CONSTRAINT IF EXISTS platform_pack_version_check,
    ADD CONSTRAINT platform_pack_version_check CHECK (version BETWEEN 1 AND 999999999);
ALTER TABLE platform_pack ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON platform_pack;
CREATE POLICY tenant_isolation ON platform_pack FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON platform_pack FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON platform_pack TO agent_app;
