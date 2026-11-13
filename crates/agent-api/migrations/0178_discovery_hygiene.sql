-- 0178 — B8 discovery hygiene (plan 2026-10-20-b8-journeys-generator-discovery T15a, F-51 / F-52; Data model
-- `0108` → `0178` per § Revision 2026-10-22 R16). Forward-only (pre-prod), drift-tolerant. Needs 0173 only.
--
-- F-51: a per-activity scorecard weight (0..1000; 100 = default = no row). Live data (D69).
-- F-52: the import's format and chosen text / count columns; deleting an import cascades its rows and clusters
-- (`discovery_decision` rows stay: their draft keys are B2's).

CREATE TABLE IF NOT EXISTS discovery_weight (
    tenant_id    uuid NOT NULL DEFAULT app_current_tenant(),
    job_id       uuid NOT NULL DEFAULT app_current_job(),
    activity_key text NOT NULL,
    weight       integer NOT NULL,
    updated_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_by   text NOT NULL,
    PRIMARY KEY (tenant_id, job_id, activity_key)
);
ALTER TABLE discovery_weight
    DROP CONSTRAINT IF EXISTS discovery_weight_job,
    ADD CONSTRAINT discovery_weight_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS discovery_weight_weight_check,
    ADD CONSTRAINT discovery_weight_weight_check CHECK (weight BETWEEN 0 AND 1000 AND weight <> 100),
    DROP CONSTRAINT IF EXISTS discovery_weight_key_check,
    ADD CONSTRAINT discovery_weight_key_check CHECK (char_length(activity_key) BETWEEN 1 AND 200),
    DROP CONSTRAINT IF EXISTS discovery_weight_updated_by_check,
    ADD CONSTRAINT discovery_weight_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE discovery_weight ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON discovery_weight;
CREATE POLICY tenant_isolation ON discovery_weight FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON discovery_weight;
CREATE POLICY job_scope ON discovery_weight AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

ALTER TABLE discovery_import
    ADD COLUMN IF NOT EXISTS format text NOT NULL DEFAULT 'csv',
    ADD COLUMN IF NOT EXISTS text_column text,
    ADD COLUMN IF NOT EXISTS count_column text;
ALTER TABLE discovery_import
    DROP CONSTRAINT IF EXISTS discovery_import_format_check,
    ADD CONSTRAINT discovery_import_format_check CHECK (format IN ('csv', 'json', 'xlsx')),
    DROP CONSTRAINT IF EXISTS discovery_import_columns_check,
    ADD CONSTRAINT discovery_import_columns_check
        CHECK ((text_column IS NULL OR char_length(text_column) BETWEEN 1 AND 1024)
               AND (count_column IS NULL OR char_length(count_column) BETWEEN 1 AND 1024));

ALTER TABLE discovery_row
    DROP CONSTRAINT IF EXISTS discovery_row_import,
    ADD CONSTRAINT discovery_row_import FOREIGN KEY (tenant_id, job_id, import_id)
        REFERENCES discovery_import (tenant_id, job_id, id) ON DELETE CASCADE;
ALTER TABLE discovery_cluster
    DROP CONSTRAINT IF EXISTS discovery_cluster_import,
    ADD CONSTRAINT discovery_cluster_import FOREIGN KEY (tenant_id, job_id, import_id)
        REFERENCES discovery_import (tenant_id, job_id, id) ON DELETE CASCADE;

REVOKE ALL ON discovery_weight FROM PUBLIC;
REVOKE ALL ON discovery_weight FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON discovery_weight TO agent_app;
REVOKE ALL ON discovery_weight FROM agent_runtime;
