-- 0148 — B5 per-job agent role settings (F-40; plan 2026-10-20-b5-job-design, Data model `0148`, T11a). Forward-only
-- (pre-prod), drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS + ADD, REVOKE ALL + GRANT, as 0047/0144/0145).
--
-- One row = the job's setting of one agent role (ledger kind `role_setting`, key `role-setting:<role>`): `enabled`
-- (the job's switch; the role runs only when the platform `agent_role` row at the design's pins is enabled too) and
-- `max_tool_calls` (the assistant's tool calls per turn, 1..6; NULL = the runtime default). No row = the platform's
-- default (off for the job). Only `assistant` may be set today (422 `role_not_configurable`, at the route); the row
-- goes when both fields are back to the default (enabled false, max NULL).
--
-- RLS: `tenant_isolation` plus a RESTRICTIVE `job_scope` (as 0060 `agent_case`, 0144).
-- Grants (R7): agent_app S/I/U/D (routes, design targets); agent_runtime SELECT (`RuntimeDesign::read`, T12, reads
-- the Test env's working rows; Production reads `release_snapshot`, granted by 0065).

CREATE TABLE IF NOT EXISTS job_agent_role_setting (
    tenant_id      uuid NOT NULL DEFAULT app_current_tenant(),
    job_id         uuid NOT NULL DEFAULT app_current_job(),
    role           text NOT NULL,
    enabled        boolean NOT NULL,
    max_tool_calls smallint NULL,
    updated_by     text NOT NULL,
    updated_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, role)
);
ALTER TABLE job_agent_role_setting
    DROP CONSTRAINT IF EXISTS job_agent_role_setting_job,
    ADD CONSTRAINT job_agent_role_setting_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS job_agent_role_setting_role_check,
    ADD CONSTRAINT job_agent_role_setting_role_check CHECK (role ~ '^[a-z_]{1,64}$'),
    DROP CONSTRAINT IF EXISTS job_agent_role_setting_max_tool_calls_check,
    ADD CONSTRAINT job_agent_role_setting_max_tool_calls_check
        CHECK (max_tool_calls IS NULL OR max_tool_calls BETWEEN 1 AND 6),
    DROP CONSTRAINT IF EXISTS job_agent_role_setting_updated_by_check,
    ADD CONSTRAINT job_agent_role_setting_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE job_agent_role_setting ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON job_agent_role_setting;
CREATE POLICY tenant_isolation ON job_agent_role_setting FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON job_agent_role_setting;
CREATE POLICY job_scope ON job_agent_role_setting AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

REVOKE ALL ON job_agent_role_setting FROM PUBLIC;
REVOKE ALL ON job_agent_role_setting FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON job_agent_role_setting TO agent_app;
REVOKE ALL ON job_agent_role_setting FROM agent_runtime;
GRANT SELECT ON job_agent_role_setting TO agent_runtime;
