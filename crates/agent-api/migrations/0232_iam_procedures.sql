-- 0232 — declared tool inputs and tenant IAM procedures (plan 2026-10-30-caller-identity-iam, A2/A3). Forward-only
-- (pre-prod), drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS + ADD, REVOKE ALL + GRANT, as 0148).
--
-- `iam_procedure`: a tenant-wide named set of tool declarations (steps) that produce conversation-state keys
-- (e.g. "Acme sign-in" -> auth_token, customer_id). No job scope; a job attaches one (`job_iam_attachment`).
-- `job_tool_policy` (ledger kind `tool_policy`, key = op key) and `job_iam_attachment` (ledger kind
-- `iam_attachment`, key = procedure id) are the WORKING rows of the two per-job design kinds; released with the
-- design through the snapshot. Shapes are validated by `runtime::declared` at the routes.
--
-- RLS: `tenant_isolation` (+ RESTRICTIVE `job_scope` on the job tables, as 0148).
-- Grants (R7): agent_app S/I/U/D; agent_runtime SELECT (test env reads working rows; prod reads release_snapshot).

CREATE TABLE IF NOT EXISTS iam_procedure (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    id          uuid NOT NULL DEFAULT uuidv7(),
    name        text NOT NULL,
    description text NOT NULL DEFAULT '',
    steps       jsonb NOT NULL DEFAULT '[]',
    created_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_by  text NOT NULL,
    PRIMARY KEY (tenant_id, id)
);
ALTER TABLE iam_procedure
    DROP CONSTRAINT IF EXISTS iam_procedure_name_check,
    ADD CONSTRAINT iam_procedure_name_check CHECK (length(name) BETWEEN 1 AND 80),
    DROP CONSTRAINT IF EXISTS iam_procedure_steps_check,
    ADD CONSTRAINT iam_procedure_steps_check CHECK (jsonb_typeof(steps) = 'array'),
    DROP CONSTRAINT IF EXISTS iam_procedure_name_unique,
    ADD CONSTRAINT iam_procedure_name_unique UNIQUE (tenant_id, name);
ALTER TABLE iam_procedure ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON iam_procedure;
CREATE POLICY tenant_isolation ON iam_procedure FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());

CREATE TABLE IF NOT EXISTS job_tool_policy (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    job_id     uuid NOT NULL DEFAULT app_current_job(),
    op         text NOT NULL,
    policy     jsonb NOT NULL,
    updated_by text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, op)
);
ALTER TABLE job_tool_policy
    DROP CONSTRAINT IF EXISTS job_tool_policy_job,
    ADD CONSTRAINT job_tool_policy_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS job_tool_policy_op_check,
    ADD CONSTRAINT job_tool_policy_op_check CHECK (length(op) BETWEEN 1 AND 200),
    DROP CONSTRAINT IF EXISTS job_tool_policy_policy_check,
    ADD CONSTRAINT job_tool_policy_policy_check CHECK (jsonb_typeof(policy) = 'object');

CREATE TABLE IF NOT EXISTS job_iam_attachment (
    tenant_id    uuid NOT NULL DEFAULT app_current_tenant(),
    job_id       uuid NOT NULL DEFAULT app_current_job(),
    procedure_id uuid NOT NULL,
    updated_by   text NOT NULL,
    updated_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, procedure_id)
);
ALTER TABLE job_iam_attachment
    DROP CONSTRAINT IF EXISTS job_iam_attachment_job,
    ADD CONSTRAINT job_iam_attachment_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS job_iam_attachment_procedure,
    ADD CONSTRAINT job_iam_attachment_procedure FOREIGN KEY (tenant_id, procedure_id)
        REFERENCES iam_procedure (tenant_id, id);

ALTER TABLE job_tool_policy ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON job_tool_policy;
CREATE POLICY tenant_isolation ON job_tool_policy FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON job_tool_policy;
CREATE POLICY job_scope ON job_tool_policy AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

ALTER TABLE job_iam_attachment ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON job_iam_attachment;
CREATE POLICY tenant_isolation ON job_iam_attachment FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON job_iam_attachment;
CREATE POLICY job_scope ON job_iam_attachment AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

REVOKE ALL ON iam_procedure, job_tool_policy, job_iam_attachment FROM PUBLIC;
REVOKE ALL ON iam_procedure, job_tool_policy, job_iam_attachment FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON iam_procedure, job_tool_policy, job_iam_attachment TO agent_app;
REVOKE ALL ON iam_procedure, job_tool_policy, job_iam_attachment FROM agent_runtime;
GRANT SELECT ON iam_procedure, job_tool_policy, job_iam_attachment TO agent_runtime;
