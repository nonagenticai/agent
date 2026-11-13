-- 0069 — the working (draft) row of B2 ledger kind `handover_policy` (B4 T16; plan Data model 0063 "the SLA policy is
-- B2 ledger kind 17", lead call: a DesignTarget needs a working row, pattern 0047 `job_setting_override`). One row per
-- job: `targets` = `HandoverSlaTargets` ({high, normal, low} × {first_response_min, resolve_min}); `hours` = the
-- F-14 business hours (T16a; NULL = always staffed). No row = no SLA. The live value is the job's release snapshot
-- (D62); this row is the draft and is written only through `design::track` (D54) or the kind's target
-- (crate::handover::policy).
--
-- Order (§0 R2): production already ran ≤0130 and applies this AFTER them; a fresh database applies it in number
-- order. It references only objects ≤0069: `jobs` (0033), `app_current_tenant()` / `app_current_job()` (0001/0033)
-- and the `agent_runtime` role (0065). Grants on later objects go to a 0131+ migration.
--
-- Drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS + ADD, REVOKE ALL + GRANT). Forward-only.

CREATE TABLE IF NOT EXISTS handover_policy (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    job_id     uuid NOT NULL,
    targets    jsonb NOT NULL,
    hours      jsonb,
    updated_by text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id)
);
ALTER TABLE handover_policy
    DROP CONSTRAINT IF EXISTS handover_policy_job,
    ADD CONSTRAINT handover_policy_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS handover_policy_targets_check,
    ADD CONSTRAINT handover_policy_targets_check CHECK (jsonb_typeof(targets) = 'object'),
    DROP CONSTRAINT IF EXISTS handover_policy_hours_check,
    ADD CONSTRAINT handover_policy_hours_check CHECK (hours IS NULL OR jsonb_typeof(hours) = 'object'),
    DROP CONSTRAINT IF EXISTS handover_policy_updated_by_check,
    ADD CONSTRAINT handover_policy_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE handover_policy ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON handover_policy;
CREATE POLICY tenant_isolation ON handover_policy FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON handover_policy;
CREATE POLICY job_scope ON handover_policy AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
REVOKE ALL ON handover_policy FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON handover_policy TO agent_app;
-- The runtime stamps the due times of a test-env item from the draft (plan R8): it reads the working row, as it reads
-- B2's other working rows (0065).
REVOKE ALL ON handover_policy FROM agent_runtime;
GRANT SELECT ON handover_policy TO agent_runtime;
