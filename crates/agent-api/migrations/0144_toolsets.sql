-- 0144 — B5 toolset narrowings (plan 2026-10-20-b5-job-design, Data model `0144`, T3). Forward-only (pre-prod),
-- drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS + ADD, as 0047).
--
-- Removal only: a row narrows one (role, activity, op) grant away; restoring = DELETE; anything else is a widening
-- (422 `widening_rejected`, at the route). Roles are `catalogue.agent_roles` at the design's pins (no copy, R1).
--
-- RLS: `tenant_isolation` plus a RESTRICTIVE `job_scope` (as 0060 `agent_case`). Grants are in 0145.

CREATE TABLE IF NOT EXISTS toolset_narrowing (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    job_id     uuid NOT NULL DEFAULT app_current_job(),
    role       text NOT NULL,
    activity   text NOT NULL,
    op         text NOT NULL,
    updated_by text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, role, activity, op)
);
ALTER TABLE toolset_narrowing
    DROP CONSTRAINT IF EXISTS toolset_narrowing_job,
    ADD CONSTRAINT toolset_narrowing_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS toolset_narrowing_op_check,
    ADD CONSTRAINT toolset_narrowing_op_check CHECK (op ~ '^[a-z_]+\.[a-z_]+(\{[^}]+\})?$'),
    DROP CONSTRAINT IF EXISTS toolset_narrowing_updated_by_check,
    ADD CONSTRAINT toolset_narrowing_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE toolset_narrowing ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON toolset_narrowing;
CREATE POLICY tenant_isolation ON toolset_narrowing FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON toolset_narrowing;
CREATE POLICY job_scope ON toolset_narrowing AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
