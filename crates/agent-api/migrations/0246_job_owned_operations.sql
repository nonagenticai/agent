-- 0246 — a tenant brings its own tools (owner 2026-10-31: the platform is general purpose; a tenant's tools are never
-- platform catalogue). Forward-only.
--
-- 1. A tenant template op whose key is in no platform catalogue carries its own definition (`def`: side_effect,
--    input_fields, output_fields, description), authored in the template editor or filled by mcp-import from the
--    MCP tool. NULL = the platform catalogue defines the op.
-- 2. `job_operation`: the ops a job adds from its tenant's connector templates (design kind `job_operation`, key
--    `job-op:<op>`), each with the identity level the tenant chose and a copy of the op's definition taken when it was
--    added, so a later template edit never changes a released job.
ALTER TABLE tenant_connector_template_op
    ADD COLUMN def jsonb CHECK (def IS NULL OR jsonb_typeof(def) = 'object');

CREATE TABLE IF NOT EXISTS job_operation (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    job_id     uuid NOT NULL DEFAULT app_current_job(),
    op         text NOT NULL,
    value      jsonb NOT NULL,
    updated_by text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, op)
);
ALTER TABLE job_operation
    DROP CONSTRAINT IF EXISTS job_operation_job,
    ADD CONSTRAINT job_operation_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS job_operation_op_check,
    ADD CONSTRAINT job_operation_op_check CHECK (length(op) BETWEEN 1 AND 200),
    DROP CONSTRAINT IF EXISTS job_operation_value_check,
    ADD CONSTRAINT job_operation_value_check CHECK (jsonb_typeof(value) = 'object');

ALTER TABLE job_operation ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON job_operation;
CREATE POLICY tenant_isolation ON job_operation FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON job_operation;
CREATE POLICY job_scope ON job_operation AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

REVOKE ALL ON job_operation FROM PUBLIC;
REVOKE ALL ON job_operation FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON job_operation TO agent_app;
REVOKE ALL ON job_operation FROM agent_runtime;
GRANT SELECT ON job_operation TO agent_runtime;
