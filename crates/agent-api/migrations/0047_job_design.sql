-- 0047 — B2's own working rows (the draft): the job's template selection, overlay refinements of the template's
-- refinable objects, job setting overrides and business-rule values (B2 T2; plan B2 §3 "0047"). Kinds `selection`,
-- `overlay`, `setting`, `rule_values`, `rule_passage` (§2.5). Written only through `design::track` (D54).
--
-- `job_overlay_row` folds care-bus-agents 0009's per-table `origin/base_hash/content_hash/hidden` overlay columns
-- into one generic table: bases are not materialised (they live in the compiled bundle, §5); a row is the tenant's
-- override, extension or tombstone of one base item. `job_setting_override` ports cb `jobs.settings_override`.
-- The rule values column is `business_rule.slot_values` (API field `values`): VALUES is a reserved word.
--
-- Drift-tolerant as 0044 (IF NOT EXISTS, DROP … IF EXISTS + ADD, REVOKE ALL + GRANT). Forward-only.

-- The job's selection (D65 Selection); NULL = none chosen yet.
CREATE TABLE IF NOT EXISTS job_target (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    job_id     uuid NOT NULL,
    selection  jsonb,
    updated_by text,
    updated_at timestamptz,
    PRIMARY KEY (tenant_id, job_id)
);
ALTER TABLE job_target
    DROP CONSTRAINT IF EXISTS job_target_job,
    ADD CONSTRAINT job_target_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS job_target_selection_check,
    ADD CONSTRAINT job_target_selection_check CHECK (selection IS NULL OR jsonb_typeof(selection) = 'object'),
    DROP CONSTRAINT IF EXISTS job_target_updated_check,
    ADD CONSTRAINT job_target_updated_check CHECK ((updated_by IS NULL) = (updated_at IS NULL));
ALTER TABLE job_target ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON job_target;
CREATE POLICY tenant_isolation ON job_target FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON job_target FROM agent_app;
GRANT SELECT, INSERT, UPDATE ON job_target TO agent_app;

-- JobTarget.history: append-only.
CREATE TABLE IF NOT EXISTS job_target_history (
    id        bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id uuid NOT NULL DEFAULT app_current_tenant(),
    job_id    uuid NOT NULL,
    at        timestamptz NOT NULL DEFAULT clock_timestamp(),
    actor     text NOT NULL,
    change    text NOT NULL,
    old       jsonb,
    new       jsonb
);
ALTER TABLE job_target_history
    DROP CONSTRAINT IF EXISTS job_target_history_job,
    ADD CONSTRAINT job_target_history_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id);
DROP INDEX IF EXISTS job_target_history_job_idx;
CREATE INDEX job_target_history_job_idx ON job_target_history (tenant_id, job_id, id);
ALTER TABLE job_target_history ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON job_target_history;
CREATE POLICY tenant_isolation ON job_target_history FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON job_target_history FROM agent_app;
GRANT SELECT, INSERT ON job_target_history TO agent_app;
GRANT USAGE ON SEQUENCE job_target_history_id_seq TO agent_app;

-- One tenant refinement of one base item of a refinable object. `item` NULL ⇔ tombstone (the base item hidden).
-- `base_hash` = row_hash of the base item it refines (NULL for an extension); `content_hash` = row_hash of `item`.
CREATE TABLE IF NOT EXISTS job_overlay_row (
    tenant_id    uuid NOT NULL DEFAULT app_current_tenant(),
    job_id       uuid NOT NULL,
    object       text NOT NULL,
    key          text NOT NULL,
    kind         text NOT NULL,
    item         jsonb,
    base_hash    text,
    content_hash text,
    updated_by   text NOT NULL,
    updated_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, object, key)
);
ALTER TABLE job_overlay_row
    DROP CONSTRAINT IF EXISTS job_overlay_row_job,
    ADD CONSTRAINT job_overlay_row_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS job_overlay_row_object_check,
    ADD CONSTRAINT job_overlay_row_object_check
        CHECK (object IN ('activity', 'context_space', 'ontology_attribute', 'gate_option', 'knowledge_store')),
    DROP CONSTRAINT IF EXISTS job_overlay_row_key_check,
    ADD CONSTRAINT job_overlay_row_key_check CHECK (char_length(key) BETWEEN 1 AND 512),
    DROP CONSTRAINT IF EXISTS job_overlay_row_kind_check,
    ADD CONSTRAINT job_overlay_row_kind_check CHECK (kind IN ('override', 'extension', 'tombstone')),
    DROP CONSTRAINT IF EXISTS job_overlay_row_item_check,
    ADD CONSTRAINT job_overlay_row_item_check CHECK ((kind = 'tombstone') = (item IS NULL)),
    DROP CONSTRAINT IF EXISTS job_overlay_row_updated_by_check,
    ADD CONSTRAINT job_overlay_row_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE job_overlay_row ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON job_overlay_row;
CREATE POLICY tenant_isolation ON job_overlay_row FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON job_overlay_row FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON job_overlay_row TO agent_app;

-- A job's override of a template setting. `value` NULL = hide the setting; no row = the base value.
CREATE TABLE IF NOT EXISTS job_setting_override (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    job_id     uuid NOT NULL,
    key        text NOT NULL,
    value      text,
    base_hash  text,
    updated_by text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, key)
);
ALTER TABLE job_setting_override
    DROP CONSTRAINT IF EXISTS job_setting_override_job,
    ADD CONSTRAINT job_setting_override_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS job_setting_override_key_check,
    ADD CONSTRAINT job_setting_override_key_check CHECK (key IN ('preamble', 'actor', 'stance')),
    DROP CONSTRAINT IF EXISTS job_setting_override_updated_by_check,
    ADD CONSTRAINT job_setting_override_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE job_setting_override ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON job_setting_override;
CREATE POLICY tenant_isolation ON job_setting_override FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON job_setting_override FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON job_setting_override TO agent_app;

-- A job's values for one template business rule: `slot_values` = {slot: string | null} (API field `values`);
-- `passage` = the PolicyPassage copy, or NULL.
CREATE TABLE IF NOT EXISTS business_rule (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant(),
    job_id      uuid NOT NULL,
    rule_id     text NOT NULL,
    slot_values jsonb NOT NULL DEFAULT '{}',
    passage     jsonb,
    updated_by  text NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, rule_id)
);
ALTER TABLE business_rule
    DROP CONSTRAINT IF EXISTS business_rule_job,
    ADD CONSTRAINT business_rule_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS business_rule_id_check,
    ADD CONSTRAINT business_rule_id_check CHECK (rule_id ~ '^[A-Z]{2,5}-[0-9]{2}$'),
    DROP CONSTRAINT IF EXISTS business_rule_slot_values_check,
    ADD CONSTRAINT business_rule_slot_values_check CHECK (jsonb_typeof(slot_values) = 'object'),
    DROP CONSTRAINT IF EXISTS business_rule_passage_check,
    ADD CONSTRAINT business_rule_passage_check CHECK (passage IS NULL OR jsonb_typeof(passage) = 'object'),
    DROP CONSTRAINT IF EXISTS business_rule_updated_by_check,
    ADD CONSTRAINT business_rule_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE business_rule ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON business_rule;
CREATE POLICY tenant_isolation ON business_rule FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON business_rule FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON business_rule TO agent_app;

-- BusinessRule.history: append-only.
CREATE TABLE IF NOT EXISTS business_rule_history (
    id        bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id uuid NOT NULL DEFAULT app_current_tenant(),
    job_id    uuid NOT NULL,
    rule_id   text NOT NULL,
    at        timestamptz NOT NULL DEFAULT clock_timestamp(),
    by_sub    text NOT NULL,
    change    text NOT NULL
);
ALTER TABLE business_rule_history
    DROP CONSTRAINT IF EXISTS business_rule_history_job,
    ADD CONSTRAINT business_rule_history_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id);
DROP INDEX IF EXISTS business_rule_history_rule_idx;
CREATE INDEX business_rule_history_rule_idx ON business_rule_history (tenant_id, job_id, rule_id, id);
ALTER TABLE business_rule_history ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON business_rule_history;
CREATE POLICY tenant_isolation ON business_rule_history FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON business_rule_history FROM agent_app;
GRANT SELECT, INSERT ON business_rule_history TO agent_app;
GRANT USAGE ON SEQUENCE business_rule_history_id_seq TO agent_app;
