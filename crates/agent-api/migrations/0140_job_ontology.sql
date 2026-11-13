-- 0140 — B5 job ontology (plan 2026-10-20-b5-job-design, Data model `0140`, T2). Forward-only (pre-prod),
-- drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS + ADD, as 0047).
--
-- Tenant rows only (R1): bases (entities, attributes, relations) are read from `compile_pinned` at the design's pins,
-- never copied here. Attributes a tenant adds to a BASE entity are B2 `ontology_attribute` overlay rows
-- (`job_overlay_row`, key `Entity.attr`), not here (R2).
--
-- RLS: `tenant_isolation` plus a RESTRICTIVE `job_scope` (as 0060 `agent_case`). Grants are in 0145.
-- Order: production has applied ≤0134; this references only `jobs`, `app_current_tenant()`, `app_current_job()`.

-- A tenant entity of one job. A name equal to a base entity at the design's pins is refused by the route.
CREATE TABLE IF NOT EXISTS ontology_entity (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    job_id     uuid NOT NULL DEFAULT app_current_job(),
    name       text NOT NULL,
    label      text NOT NULL,
    scope      text NOT NULL,
    industry   text,
    department text,
    synonyms   text[] NOT NULL DEFAULT '{}',
    attributes jsonb NOT NULL DEFAULT '[]',
    hidden     boolean NOT NULL DEFAULT false,
    object_key text,
    created_by text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, name)
);
ALTER TABLE ontology_entity
    DROP CONSTRAINT IF EXISTS ontology_entity_job,
    ADD CONSTRAINT ontology_entity_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS ontology_entity_name_check,
    ADD CONSTRAINT ontology_entity_name_check CHECK (name ~ '^[A-Z][A-Za-z0-9]{0,63}$'),
    DROP CONSTRAINT IF EXISTS ontology_entity_label_check,
    ADD CONSTRAINT ontology_entity_label_check CHECK (char_length(label) BETWEEN 1 AND 120),
    DROP CONSTRAINT IF EXISTS ontology_entity_scope_check,
    ADD CONSTRAINT ontology_entity_scope_check CHECK (scope IN ('generic', 'industry', 'industry_dept')),
    DROP CONSTRAINT IF EXISTS ontology_entity_attributes_check,
    ADD CONSTRAINT ontology_entity_attributes_check CHECK (jsonb_typeof(attributes) = 'array'),
    DROP CONSTRAINT IF EXISTS ontology_entity_created_by_check,
    ADD CONSTRAINT ontology_entity_created_by_check CHECK (btrim(created_by) <> '');
ALTER TABLE ontology_entity ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON ontology_entity;
CREATE POLICY tenant_isolation ON ontology_entity FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON ontology_entity;
CREATE POLICY job_scope ON ontology_entity AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- A tenant refinement of a BASE entity (B2 does not overlay entities). Deleted when both fields are back to the base.
-- `base_hash` = `release::hash::row_hash` of the base entity it refines (T4 conflict detection).
CREATE TABLE IF NOT EXISTS ontology_entity_refinement (
    tenant_id      uuid NOT NULL DEFAULT app_current_tenant(),
    job_id         uuid NOT NULL DEFAULT app_current_job(),
    name           text NOT NULL,
    hidden         boolean NOT NULL DEFAULT false,
    synonyms_added text[] NOT NULL DEFAULT '{}',
    base_hash      text NOT NULL,
    updated_by     text NOT NULL,
    updated_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, name)
);
ALTER TABLE ontology_entity_refinement
    DROP CONSTRAINT IF EXISTS ontology_entity_refinement_job,
    ADD CONSTRAINT ontology_entity_refinement_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS ontology_entity_refinement_name_check,
    ADD CONSTRAINT ontology_entity_refinement_name_check CHECK (name ~ '^[A-Z][A-Za-z0-9]{0,63}$'),
    DROP CONSTRAINT IF EXISTS ontology_entity_refinement_base_hash_check,
    ADD CONSTRAINT ontology_entity_refinement_base_hash_check CHECK (btrim(base_hash) <> ''),
    DROP CONSTRAINT IF EXISTS ontology_entity_refinement_updated_by_check,
    ADD CONSTRAINT ontology_entity_refinement_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE ontology_entity_refinement ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON ontology_entity_refinement;
CREATE POLICY tenant_isolation ON ontology_entity_refinement FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON ontology_entity_refinement;
CREATE POLICY job_scope ON ontology_entity_refinement AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- A tenant relation of one job. Base relations are not rows (deleting one is 409 `base_row`).
CREATE TABLE IF NOT EXISTS ontology_relation (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant(),
    job_id      uuid NOT NULL DEFAULT app_current_job(),
    subject     text NOT NULL,
    predicate   text NOT NULL,
    object      text NOT NULL,
    cardinality text NOT NULL,
    created_by  text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, subject, predicate, object)
);
ALTER TABLE ontology_relation
    DROP CONSTRAINT IF EXISTS ontology_relation_job,
    ADD CONSTRAINT ontology_relation_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS ontology_relation_predicate_check,
    ADD CONSTRAINT ontology_relation_predicate_check CHECK (predicate ~ '^[a-z][A-Za-z0-9]{0,63}$'),
    DROP CONSTRAINT IF EXISTS ontology_relation_cardinality_check,
    ADD CONSTRAINT ontology_relation_cardinality_check CHECK (cardinality IN ('1:1', '1:n', 'n:1', 'n:m')),
    DROP CONSTRAINT IF EXISTS ontology_relation_created_by_check,
    ADD CONSTRAINT ontology_relation_created_by_check CHECK (btrim(created_by) <> '');
ALTER TABLE ontology_relation ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON ontology_relation;
CREATE POLICY tenant_isolation ON ontology_relation FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON ontology_relation;
CREATE POLICY job_scope ON ontology_relation AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
